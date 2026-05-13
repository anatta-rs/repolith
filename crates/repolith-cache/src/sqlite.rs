//! SQLite-backed [`Cache`] implementation.
//!
//! Wraps a single `rusqlite::Connection` behind `Arc<Mutex<>>` and runs every
//! statement inside `tokio::task::spawn_blocking` because `rusqlite` is
//! synchronous + `!Send`. Schema is the single CREATE TABLE in
//! `schema.sql` (no migrations in M1 — `IF NOT EXISTS` is the upgrade path).
//!
//! `BuildEvent::Failed.error` is stored as JSON in the `error_json` column so
//! the typed `BuildError` survives the roundtrip without lossy stringification.

use async_trait::async_trait;
use repolith_core::cache::{Cache, CacheError, Result};
use repolith_core::types::{ActionId, BuildError, BuildEvent, Sha256};
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

const SCHEMA: &str = include_str!("schema.sql");

/// `SQLite` cache backend. Cheap to clone (`Arc<Mutex<Connection>>` shared).
pub struct SqliteCache {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteCache {
    /// Open (or create) the cache database at `path`. Creates the parent
    /// directory if needed and applies the schema on first use.
    ///
    /// # Errors
    /// - [`CacheError::Io`] when the parent directory cannot be created.
    /// - [`CacheError::Backend`] when `rusqlite` cannot open the file or
    ///   apply the schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).map_err(|e| CacheError::Backend(e.to_string()))?;
        Self::configure_pragmas(&conn)?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| CacheError::Backend(e.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory cache. Convenient for tests; data is lost on drop.
    ///
    /// # Errors
    /// [`CacheError::Backend`] when `rusqlite` cannot open the in-memory
    /// connection or apply the schema (extremely unlikely in practice).
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| CacheError::Backend(e.to_string()))?;
        // WAL is meaningless for `:memory:` databases, but `busy_timeout`
        // and `synchronous=NORMAL` are still applied for consistency with
        // file-backed caches.
        Self::configure_pragmas(&conn)?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| CacheError::Backend(e.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Apply the standard pragma set so concurrent `repolith sync`
    /// invocations against the same cache file don't deadlock with
    /// `SQLITE_BUSY`.
    ///
    /// - `journal_mode = WAL` — readers don't block writers, writers don't
    ///   block readers. The `-wal` and `-shm` sidecar files appear next to
    ///   `cache.db` automatically.
    /// - `synchronous = NORMAL` — durable enough for a build cache (no
    ///   per-commit fsync), an order of magnitude faster than `FULL`.
    /// - `busy_timeout = 5000` — wait up to 5 s on lock contention before
    ///   returning `SQLITE_BUSY`.
    fn configure_pragmas(conn: &Connection) -> Result<()> {
        // `journal_mode` returns the new mode; we ignore the result but the
        // `query_row` call is what actually drives the pragma.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .map_err(|e| CacheError::Backend(format!("set journal_mode=WAL: {e}")))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| CacheError::Backend(format!("set synchronous=NORMAL: {e}")))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| CacheError::Backend(format!("set busy_timeout: {e}")))?;
        Ok(())
    }
}

#[async_trait]
impl Cache for SqliteCache {
    async fn last_build(&self, id: &ActionId) -> Option<BuildEvent> {
        let conn = Arc::clone(&self.conn);
        let id = id.clone();
        tokio::task::spawn_blocking(move || -> Option<BuildEvent> {
            let c = conn.lock().ok()?;
            c.query_row(
                "SELECT input_hash, output_hash, status, started_at, ended_at, error_json
                 FROM build_events WHERE action_id = ?1",
                params![id.0],
                |row| {
                    let input_hex: String = row.get(0)?;
                    let status: String = row.get(2)?;
                    let started: i64 = row.get(3)?;
                    let ended: i64 = row.get(4)?;
                    let ms = u64::try_from((ended - started).max(0)).unwrap_or(0);

                    let input = parse_sha256(&input_hex).ok_or_else(|| {
                        rusqlite::Error::InvalidColumnType(
                            0,
                            "input_hash".into(),
                            rusqlite::types::Type::Text,
                        )
                    })?;

                    Ok(if status == "success" {
                        let out_hex: String = row.get(1)?;
                        let output = parse_sha256(&out_hex).ok_or_else(|| {
                            rusqlite::Error::InvalidColumnType(
                                1,
                                "output_hash".into(),
                                rusqlite::types::Type::Text,
                            )
                        })?;
                        BuildEvent::Success {
                            id: id.clone(),
                            input,
                            output,
                            ms,
                        }
                    } else {
                        let err_json: Option<String> = row.get(5)?;
                        let error = err_json
                            .and_then(|s| serde_json::from_str::<BuildError>(&s).ok())
                            .unwrap_or(BuildError::Cancelled);
                        BuildEvent::Failed {
                            id: id.clone(),
                            input,
                            error,
                            ms,
                        }
                    })
                },
            )
            .ok()
        })
        .await
        .ok()
        .flatten()
    }

    async fn record(&mut self, ev: BuildEvent) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let c = conn
                .lock()
                .map_err(|_| CacheError::Backend("connection mutex poisoned".into()))?;
            match ev {
                BuildEvent::Success {
                    id,
                    input,
                    output,
                    ms,
                } => {
                    c.execute(
                        "INSERT OR REPLACE INTO build_events
                         (action_id, input_hash, output_hash, status, started_at, ended_at, error_json)
                         VALUES (?1, ?2, ?3, 'success', 0, ?4, NULL)",
                        params![
                            id.0,
                            input.to_string(),
                            output.to_string(),
                            i64::try_from(ms).unwrap_or(i64::MAX)
                        ],
                    )
                    .map_err(|e| CacheError::Backend(e.to_string()))?;
                }
                BuildEvent::Failed {
                    id,
                    input,
                    error,
                    ms,
                } => {
                    let err_json = serde_json::to_string(&error)
                        .map_err(|e| CacheError::Backend(format!("serialize error: {e}")))?;
                    c.execute(
                        "INSERT OR REPLACE INTO build_events
                         (action_id, input_hash, output_hash, status, started_at, ended_at, error_json)
                         VALUES (?1, ?2, NULL, 'failed', 0, ?3, ?4)",
                        params![
                            id.0,
                            input.to_string(),
                            i64::try_from(ms).unwrap_or(i64::MAX),
                            err_json
                        ],
                    )
                    .map_err(|e| CacheError::Backend(e.to_string()))?;
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| CacheError::Backend(format!("spawn_blocking join: {e}")))?
    }
}

fn parse_sha256(s: &str) -> Option<Sha256> {
    let bytes = hex::decode(s).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    Some(Sha256(arr))
}
