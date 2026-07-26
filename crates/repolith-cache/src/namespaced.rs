//! `NamespacedCache` — decorator that prefixes every `ActionId` before
//! it reaches the inner backend.
//!
//! This is the bridge between federation and a **shared** cache: two child
//! stacks that both declare a node `api` would collide in a single Neo4j
//! database; wrapping each child's handle in
//! `NamespacedCache::new(inner, "parent-node::")` keeps their keys
//! disjoint. Local per-stack `SQLite` caches (the federation default) do
//! not need this — separate files cannot collide.

use async_trait::async_trait;
use repolith_core::cache::{Cache, Result};
use repolith_core::types::{ActionId, BuildEvent};

/// Cache decorator that prefixes every id with a fixed namespace.
pub struct NamespacedCache<C> {
    inner: C,
    prefix: String,
}

impl<C: Cache> NamespacedCache<C> {
    /// Wrap `inner`, prefixing every id with `prefix` (e.g. `"stack-a::"`).
    pub fn new(inner: C, prefix: impl Into<String>) -> Self {
        Self {
            inner,
            prefix: prefix.into(),
        }
    }

    fn prefixed(&self, id: &ActionId) -> ActionId {
        ActionId(format!("{}{}", self.prefix, id.0))
    }

    /// Rewrite the event's id with the namespace prefix.
    fn prefixed_event(&self, event: BuildEvent) -> BuildEvent {
        match event {
            BuildEvent::Success {
                id,
                input,
                output,
                ms,
            } => BuildEvent::Success {
                id: self.prefixed(&id),
                input,
                output,
                ms,
            },
            BuildEvent::Failed {
                id,
                input,
                error,
                ms,
            } => BuildEvent::Failed {
                id: self.prefixed(&id),
                input,
                error,
                ms,
            },
        }
    }

    /// Strip the prefix from an id coming back out of the inner backend so
    /// callers see the same ids they stored.
    fn stripped_event(&self, event: BuildEvent) -> BuildEvent {
        let strip = |id: ActionId| {
            ActionId(
                id.0.strip_prefix(&self.prefix)
                    .map_or_else(|| id.0.clone(), std::string::ToString::to_string),
            )
        };
        match event {
            BuildEvent::Success {
                id,
                input,
                output,
                ms,
            } => BuildEvent::Success {
                id: strip(id),
                input,
                output,
                ms,
            },
            BuildEvent::Failed {
                id,
                input,
                error,
                ms,
            } => BuildEvent::Failed {
                id: strip(id),
                input,
                error,
                ms,
            },
        }
    }
}

#[async_trait]
impl<C: Cache> Cache for NamespacedCache<C> {
    async fn last_build(&self, id: &ActionId) -> Option<BuildEvent> {
        let event = self.inner.last_build(&self.prefixed(id)).await?;
        Some(self.stripped_event(event))
    }

    async fn record(&mut self, event: BuildEvent) -> Result<()> {
        let event = self.prefixed_event(event);
        self.inner.record(event).await
    }

    async fn record_batch(&mut self, events: Vec<BuildEvent>) -> Result<()> {
        let events = events.into_iter().map(|e| self.prefixed_event(e)).collect();
        self.inner.record_batch(events).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteCache;
    use repolith_core::types::Sha256;

    fn success(id: &str) -> BuildEvent {
        BuildEvent::Success {
            id: ActionId(id.to_string()),
            input: Sha256([1; 32]),
            output: Sha256([2; 32]),
            ms: 5,
        }
    }

    #[tokio::test]
    async fn namespaces_are_disjoint_on_a_shared_backend() {
        // Two namespaces over the SAME inner cache must not see each other.
        let dir = tempfile::tempdir().unwrap();
        let shared = SqliteCache::open(dir.path().join("c.db")).unwrap();
        let mut a = NamespacedCache::new(shared.clone(), "stack-a::");
        let mut b = NamespacedCache::new(shared, "stack-b::");
        let id = ActionId("api::git-clone::0".to_string());

        a.record(success(&id.0)).await.unwrap();
        assert!(a.last_build(&id).await.is_some(), "a sees its own event");
        assert!(
            b.last_build(&id).await.is_none(),
            "b must not see a's event"
        );

        b.record(success(&id.0)).await.unwrap();
        assert!(b.last_build(&id).await.is_some());
    }

    #[tokio::test]
    async fn ids_roundtrip_unprefixed() {
        let mut c = NamespacedCache::new(SqliteCache::in_memory().unwrap(), "ns::");
        let id = ActionId("node::kind::0".to_string());
        c.record(success(&id.0)).await.unwrap();
        match c.last_build(&id).await.unwrap() {
            BuildEvent::Success { id: got, .. } => assert_eq!(got, id, "prefix must be stripped"),
            BuildEvent::Failed { .. } => panic!("expected Success"),
        }
    }
}
