//! Cache trait — abstract storage for build events.
//!
//! Defines the contract that any backend (sqlite, in-memory, redis…) must
//! satisfy. Concrete implementations live in dependent crates (e.g.
//! `repolith-cache::SqliteCache`). The trait lives here in `repolith-core`
//! so consumers like [`crate::plan::Plan`] can depend on the abstraction
//! without pulling in any specific backend.

use crate::types::{ActionId, BuildEvent};
use async_trait::async_trait;
use thiserror::Error;

/// Errors that can occur during cache operations.
#[derive(Debug, Error)]
pub enum CacheError {
    /// An I/O error occurred while accessing the cache.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A backend-specific error occurred (driver-level, schema, lock, …).
    #[error("backend: {0}")]
    Backend(String),
}

/// A specialized `Result` type for cache operations.
pub type Result<T> = std::result::Result<T, CacheError>;

/// Persistent store for `BuildEvent`s, keyed by [`ActionId`].
///
/// The cache lets the [planner](crate::plan::Plan) compare current input
/// hashes against the last recorded build, and lets the orchestrator persist
/// new events as they fire.
#[async_trait]
pub trait Cache: Send + Sync {
    /// Last recorded event for `id`, or `None` if the action has never run.
    async fn last_build(&self, id: &ActionId) -> Option<BuildEvent>;

    /// Persist `event`. After success, [`Self::last_build`] for the event's
    /// id must return this event.
    async fn record(&mut self, event: BuildEvent) -> Result<()>;

    /// Persist `events` atomically — either every event lands or none.
    ///
    /// The orchestrator calls this at the end of each layer so a crash
    /// mid-batch doesn't leave half the layer marked Success and the
    /// other half un-persisted (which would re-run those actions on
    /// the next sync, contradicting the cached Success).
    ///
    /// The default implementation is a non-atomic loop over [`Self::record`]
    /// that suffices for in-memory backends; persistent backends
    /// (`SQLite`, etc.) should override with a transaction.
    async fn record_batch(&mut self, events: Vec<BuildEvent>) -> Result<()> {
        for event in events {
            self.record(event).await?;
        }
        Ok(())
    }
}
