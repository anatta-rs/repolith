//! Neo4j-backed `Cache` implementation (feature `neo4j`).
//!
//! Build events become graph data: one `(:Action {id})` node per action,
//! merged on write, holding a single `LAST` relationship to the most
//! recent `(:BuildEvent)` — exactly the contract the `Cache` trait
//! requires (`last_build` returns the latest event only). Full history is
//! a deliberate non-goal for now; it can be added later without breaking
//! this schema (keep old events, move the `LAST` relationship).
//!
//! A shared server turns every cache probe into a network round-trip —
//! this backend is for **multi-machine / federated** stacks where sharing
//! beats latency; `SqliteCache` stays the default.
//!
//! # Security
//!
//! Credentials come from the environment only (`REPOLITH_NEO4J_URI`,
//! `REPOLITH_NEO4J_USER`, `REPOLITH_NEO4J_PASS`) — never from
//! `repolith.toml`, so a malicious manifest cannot redirect cache writes.
//! Connection errors are redacted: the password never appears in any
//! error message or log line. The initial ping is bounded by
//! `CONNECT_TIMEOUT` so an unreachable server fails fast instead of
//! hanging a sync.

use async_trait::async_trait;
use neo4rs::{Graph, query};
use repolith_core::cache::{Cache, CacheError, Result};
use repolith_core::types::{ActionId, BuildError, BuildEvent, BuildRecord, Sha256};
use std::time::Duration;

/// Upper bound on the initial connectivity check.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Environment variable names for the connection settings.
pub const ENV_URI: &str = "REPOLITH_NEO4J_URI";
/// See [`ENV_URI`].
pub const ENV_USER: &str = "REPOLITH_NEO4J_USER";
/// See [`ENV_URI`].
pub const ENV_PASS: &str = "REPOLITH_NEO4J_PASS";

/// Connection settings for [`Neo4jCache::connect`].
pub struct Neo4jConfig {
    /// Bolt URI, e.g. `bolt://localhost:7687`.
    pub uri: String,
    /// Username.
    pub user: String,
    /// Password. Held only long enough to open the driver; never logged.
    pub pass: String,
}

impl Neo4jConfig {
    /// Read the connection settings from the environment
    /// (`REPOLITH_NEO4J_URI` / `_USER` / `_PASS`).
    ///
    /// # Errors
    /// [`CacheError::Backend`] naming the missing variable (its *name*,
    /// never a value).
    pub fn from_env() -> Result<Self> {
        let get = |k: &str| {
            std::env::var(k).map_err(|_| {
                CacheError::Backend(format!("neo4j cache backend requires the `{k}` env var"))
            })
        };
        Ok(Self {
            uri: get(ENV_URI)?,
            user: get(ENV_USER)?,
            pass: get(ENV_PASS)?,
        })
    }
}

/// Neo4j cache backend. Cheap to clone (`Graph` is an `Arc`'d pool).
#[derive(Clone)]
pub struct Neo4jCache {
    graph: Graph,
}

impl Neo4jCache {
    /// Connect and verify reachability with a bounded ping.
    ///
    /// # Errors
    /// [`CacheError::Backend`] on driver construction failure, on ping
    /// failure (auth, network — message redacted: no password), or when
    /// the ping exceeds [`CONNECT_TIMEOUT`].
    pub async fn connect(cfg: &Neo4jConfig) -> Result<Self> {
        let connect = Graph::new(&cfg.uri, &cfg.user, &cfg.pass);
        let graph = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(Ok(g)) => g,
            Ok(Err(e)) => {
                return Err(CacheError::Backend(format!(
                    "neo4j connect failed for {}: {}",
                    cfg.uri,
                    redact(&e.to_string(), &cfg.pass)
                )));
            }
            Err(_) => {
                return Err(CacheError::Backend(format!(
                    "neo4j unreachable at {} (timed out after {CONNECT_TIMEOUT:?})",
                    cfg.uri
                )));
            }
        };
        let ping = graph.run(query("RETURN 1"));
        match tokio::time::timeout(CONNECT_TIMEOUT, ping).await {
            Ok(Ok(())) => Ok(Self { graph }),
            Ok(Err(e)) => Err(CacheError::Backend(format!(
                "neo4j ping failed for {}: {}",
                cfg.uri,
                redact(&e.to_string(), &cfg.pass)
            ))),
            Err(_) => Err(CacheError::Backend(format!(
                "neo4j unreachable at {} (timed out after {CONNECT_TIMEOUT:?})",
                cfg.uri
            ))),
        }
    }

    fn upsert_query(ev: &BuildEvent) -> neo4rs::Query {
        let (id, input, output, error_json, kind, ms) = match ev {
            BuildEvent::Success {
                id,
                input,
                output,
                ms,
            } => (
                id.0.clone(),
                input.to_string(),
                output.to_string(),
                String::new(),
                "success",
                i64::try_from(*ms).unwrap_or(i64::MAX),
            ),
            BuildEvent::Failed {
                id,
                input,
                error,
                ms,
            } => (
                id.0.clone(),
                input.to_string(),
                String::new(),
                serde_json::to_string(error).unwrap_or_default(),
                "failed",
                i64::try_from(*ms).unwrap_or(i64::MAX),
            ),
        };
        query(
            "MERGE (a:Action {id: $id})
             WITH a
             OPTIONAL MATCH (a)-[r:LAST]->(old:BuildEvent)
             DELETE r
             DETACH DELETE old
             WITH a
             CREATE (a)-[:LAST]->(:BuildEvent {
                 kind: $kind, input: $input, output: $output,
                 error_json: $error_json, ms: $ms, recorded_at: timestamp()
             })",
        )
        .param("id", id)
        .param("kind", kind)
        .param("input", input)
        .param("output", output)
        .param("error_json", error_json)
        .param("ms", ms)
    }
}

/// Replace any occurrence of the password in `msg` — drivers sometimes
/// echo connection strings back in error text.
fn redact(msg: &str, pass: &str) -> String {
    if pass.is_empty() {
        msg.to_string()
    } else {
        msg.replace(pass, "***")
    }
}

fn parse_sha256(s: &str) -> Option<Sha256> {
    let bytes = hex::decode(s).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    Some(Sha256(arr))
}

impl Neo4jCache {
    /// The single read path, shared by [`Cache::last_build`] and
    /// [`Cache::last_record`] so the two can never drift apart.
    async fn fetch(&self, id: &ActionId) -> Option<BuildRecord> {
        let q = query(
            "MATCH (a:Action {id: $id})-[:LAST]->(e:BuildEvent)
             RETURN e.kind AS kind, e.input AS input, e.output AS output,
                    e.error_json AS error_json, e.ms AS ms,
                    e.recorded_at AS recorded_at",
        )
        .param("id", id.0.clone());
        let mut rows = self.graph.execute(q).await.ok()?;
        let row = rows.next().await.ok()??;

        let kind: String = row.get("kind").ok()?;
        let input = parse_sha256(&row.get::<String>("input").ok()?)?;
        let ms = u64::try_from(row.get::<i64>("ms").ok()?).unwrap_or(0);
        // Nodes created before `recorded_at` was written have no such
        // property; a missing or negative value means "unknown", never zero.
        let recorded_at = row.get::<i64>("recorded_at").ok().and_then(|t| {
            let t = u64::try_from(t).ok()?;
            (t != 0).then_some(t)
        });

        let event = if kind == "success" {
            let output = parse_sha256(&row.get::<String>("output").ok()?)?;
            BuildEvent::Success {
                id: id.clone(),
                input,
                output,
                ms,
            }
        } else {
            let error = serde_json::from_str::<BuildError>(&row.get::<String>("error_json").ok()?)
                .unwrap_or(BuildError::Cancelled);
            BuildEvent::Failed {
                id: id.clone(),
                input,
                error,
                ms,
            }
        };
        Some(BuildRecord { event, recorded_at })
    }
}

#[async_trait]
impl Cache for Neo4jCache {
    async fn last_build(&self, id: &ActionId) -> Option<BuildEvent> {
        self.fetch(id).await.map(|r| r.event)
    }

    async fn last_record(&self, id: &ActionId) -> Option<BuildRecord> {
        self.fetch(id).await
    }

    async fn record(&mut self, event: BuildEvent) -> Result<()> {
        self.graph
            .run(Self::upsert_query(&event))
            .await
            .map_err(|e| CacheError::Backend(format!("neo4j record: {e}")))
    }

    /// One explicit transaction for the whole layer — either every event
    /// lands or none, matching the trait's atomicity contract.
    async fn record_batch(&mut self, events: Vec<BuildEvent>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut txn = self
            .graph
            .start_txn()
            .await
            .map_err(|e| CacheError::Backend(format!("neo4j begin txn: {e}")))?;
        for ev in &events {
            if let Err(e) = txn.run(Self::upsert_query(ev)).await {
                // Best-effort rollback; the commit never happened either way.
                let _ = txn.rollback().await;
                return Err(CacheError::Backend(format!("neo4j batch write: {e}")));
            }
        }
        txn.commit()
            .await
            .map_err(|e| CacheError::Backend(format!("neo4j commit: {e}")))
    }
}
