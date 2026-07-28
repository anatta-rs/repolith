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
use repolith_core::types::{ActionId, BuildError, BuildEvent, BuildRecord, Sha256};
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

const SCHEMA: &str = include_str!("schema.sql");

/// `SQLite` cache backend. Cheap to clone (`Arc<Mutex<Connection>>` shared).
#[derive(Clone)]
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

    /// The single read path, shared by [`Cache::last_build`] and
    /// [`Cache::last_record`] so the two can never drift apart.
    async fn fetch(&self, id: &ActionId) -> Option<BuildRecord> {
        let conn = Arc::clone(&self.conn);
        let id = id.clone();
        tokio::task::spawn_blocking(move || -> Option<BuildRecord> {
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

                    // `started_at == 0` is the "no write time" marker: it is
                    // what every row written before this column carried a
                    // real clock reading looks like, and what `insert_event`
                    // still falls back to when the clock is unusable. Such a
                    // row must report absence, never an epoch-zero date.
                    let recorded_at = if started == 0 {
                        None
                    } else {
                        u64::try_from(ended).ok()
                    };

                    let input = parse_sha256(&input_hex).ok_or_else(|| {
                        rusqlite::Error::InvalidColumnType(
                            0,
                            "input_hash".into(),
                            rusqlite::types::Type::Text,
                        )
                    })?;

                    let event = if status == "success" {
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
                    };
                    Ok(BuildRecord { event, recorded_at })
                },
            )
            .ok()
        })
        .await
        .ok()
        .flatten()
    }
}

#[async_trait]
impl Cache for SqliteCache {
    async fn last_build(&self, id: &ActionId) -> Option<BuildEvent> {
        let hit = self.fetch(id).await.map(|r| r.event);
        tracing::trace!(%id, hit = hit.is_some(), "cache lookup");
        hit
    }

    async fn last_record(&self, id: &ActionId) -> Option<BuildRecord> {
        self.fetch(id).await
    }

    async fn record(&mut self, ev: BuildEvent) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let c = conn
                .lock()
                .map_err(|_| CacheError::Backend("connection mutex poisoned".into()))?;
            insert_event(&c, &ev)
        })
        .await
        .map_err(|e| CacheError::Backend(format!("spawn_blocking join: {e}")))?
    }

    /// Override the default `Cache::record_batch` impl with a single
    /// `SQLite` transaction so the whole layer's events land atomically.
    /// Without this the default loops `record` and any crash mid-batch
    /// leaves the cache half-updated.
    async fn record_batch(&mut self, events: Vec<BuildEvent>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut c = conn
                .lock()
                .map_err(|_| CacheError::Backend("connection mutex poisoned".into()))?;
            let tx = c
                .transaction()
                .map_err(|e| CacheError::Backend(format!("begin transaction: {e}")))?;
            for ev in &events {
                insert_event(&tx, ev)?;
            }
            tx.commit()
                .map_err(|e| CacheError::Backend(format!("commit transaction: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| CacheError::Backend(format!("spawn_blocking join: {e}")))?
    }
}

/// Insert (or replace) one [`BuildEvent`] row using `conn`. Pulled out
/// of [`SqliteCache::record`] so [`SqliteCache::record_batch`] can run
/// the same insertion inside a transaction.
fn insert_event(conn: &Connection, ev: &BuildEvent) -> Result<()> {
    match ev {
        BuildEvent::Success {
            id,
            input,
            output,
            ms,
        } => {
            let (started, ended) = stamps(*ms);
            conn.execute(
                "INSERT OR REPLACE INTO build_events
                 (action_id, input_hash, output_hash, status, started_at, ended_at, error_json)
                 VALUES (?1, ?2, ?3, 'success', ?4, ?5, NULL)",
                params![id.0, input.to_string(), output.to_string(), started, ended],
            )
            .map_err(|e| CacheError::Backend(e.to_string()))?;
        }
        BuildEvent::Failed {
            id,
            input,
            error,
            ms,
        } => {
            let err_json = serde_json::to_string(error)
                .map_err(|e| CacheError::Backend(format!("serialize error: {e}")))?;
            let (started, ended) = stamps(*ms);
            conn.execute(
                "INSERT OR REPLACE INTO build_events
                 (action_id, input_hash, output_hash, status, started_at, ended_at, error_json)
                 VALUES (?1, ?2, NULL, 'failed', ?3, ?4, ?5)",
                params![id.0, input.to_string(), started, ended, err_json],
            )
            .map_err(|e| CacheError::Backend(e.to_string()))?;
        }
    }
    Ok(())
}

/// The `(started_at, ended_at)` pair to store for a build that took `ms`.
///
/// Anchored on the wall clock so `ended_at` is a genuine timestamp — the
/// columns previously held `(0, ms)`, which made them lie about their own
/// names and left the backend unable to say when anything ran.
///
/// The subtraction is deliberate: the reader recovers the duration as
/// `ended - started`, so anchoring the *end* on the clock keeps that
/// invariant exactly intact while making the stored end date true. No
/// migration is needed, and no existing row changes meaning.
///
/// A clock that cannot be read, or that places "now" at or before this
/// build's own duration, is not one we can date anything with. Both degrade
/// to the legacy `(0, ms)` shape, which the reader already interprets as
/// "no write time" — so a broken clock costs a missing date, never a wrong
/// duration and never a 1970 date.
fn stamps(ms: u64) -> (i64, i64) {
    let ms = i64::try_from(ms).unwrap_or(i64::MAX);
    let legacy = (0, ms);
    let Ok(since_epoch) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return legacy;
    };
    let ended = i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX);
    if ended <= ms {
        legacy
    } else {
        (ended - ms, ended)
    }
}

fn parse_sha256(s: &str) -> Option<Sha256> {
    let bytes = hex::decode(s).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    Some(Sha256(arr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use repolith_core::types::ActionId;

    fn ok_event(id: &str, ms: u64) -> BuildEvent {
        BuildEvent::Success {
            id: ActionId(id.to_string()),
            input: Sha256([3; 32]),
            output: Sha256([4; 32]),
            ms,
        }
    }

    /// Write a row exactly as every version before this fix did: the
    /// `started_at` column pinned to 0, the duration parked in `ended_at`.
    fn insert_legacy_row(cache: &SqliteCache, id: &str, ms: i64) {
        let c = cache.conn.lock().expect("lock");
        c.execute(
            "INSERT OR REPLACE INTO build_events
             (action_id, input_hash, output_hash, status, started_at, ended_at, error_json)
             VALUES (?1, ?2, ?3, 'success', 0, ?4, NULL)",
            params![
                id,
                Sha256([3; 32]).to_string(),
                Sha256([4; 32]).to_string(),
                ms
            ],
        )
        .expect("insert legacy row");
    }

    fn columns_of(cache: &SqliteCache, id: &str) -> (i64, i64) {
        let c = cache.conn.lock().expect("lock");
        c.query_row(
            "SELECT started_at, ended_at FROM build_events WHERE action_id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("row exists")
    }

    /// The whole point of not migrating: a cache written by an older
    /// repolith must keep working, and must not surface a 1970 date.
    #[tokio::test]
    async fn legacy_row_keeps_its_duration_and_reports_no_date() {
        let cache = SqliteCache::in_memory().expect("in-memory");
        insert_legacy_row(&cache, "old::cargo-install::0", 1234);
        let id = ActionId("old::cargo-install::0".to_string());

        let rec = cache
            .last_record(&id)
            .await
            .expect("a pre-fix row must stay readable");
        assert!(
            rec.recorded_at.is_none(),
            "started_at == 0 is the pre-fix shape; it means 'unknown', not epoch zero"
        );
        match rec.event {
            BuildEvent::Success { ms, .. } => {
                assert_eq!(
                    ms, 1234,
                    "the duration stored by the old writer must survive"
                );
            }
            BuildEvent::Failed { .. } => panic!("expected Success"),
        }
    }

    #[tokio::test]
    async fn new_rows_are_dated_without_disturbing_the_duration() {
        let mut cache = SqliteCache::in_memory().expect("in-memory");
        let id = ActionId("new::cargo-install::0".to_string());
        cache.record(ok_event(&id.0, 1234)).await.expect("record");

        let rec = cache.last_record(&id).await.expect("readable");
        match rec.event {
            BuildEvent::Success { ms, .. } => assert_eq!(ms, 1234, "duration must be untouched"),
            BuildEvent::Failed { .. } => panic!("expected Success"),
        }
        assert!(
            rec.recorded_at.is_some(),
            "a freshly written row must be dated"
        );

        // Assert on the raw columns too: the read path recovers `ms` by
        // subtraction, so a writer that stored (0, ms) would pass every
        // assertion above while still leaving the columns meaningless.
        let (started, ended) = columns_of(&cache, &id.0);
        assert_eq!(ended - started, 1234, "duration recoverable by subtraction");
        assert!(
            started > 1_600_000_000_000,
            "started_at must be a real epoch-ms reading (2020 or later), got {started}"
        );
    }

    #[test]
    fn stamps_preserve_the_subtraction_invariant() {
        for ms in [0u64, 1, 1234, 86_400_000] {
            let (started, ended) = stamps(ms);
            assert_eq!(
                u64::try_from(ended - started).expect("non-negative"),
                ms,
                "last_build recovers the duration as ended - started; \
                 stamps() must keep that exact for ms = {ms}"
            );
        }
    }

    #[test]
    fn an_undatable_build_falls_back_to_the_legacy_shape() {
        // A duration larger than the current epoch time cannot be anchored
        // on the clock without inventing a pre-epoch start. Degrade to the
        // undated shape instead — a missing date, never a wrong duration.
        let (started, ended) = stamps(u64::MAX);
        assert_eq!(started, 0, "0 is the 'no write time' marker");
        assert_eq!(ended - started, i64::MAX, "duration still recoverable");
    }
}
