//! Caching traits and implementations for the repolith orchestration engine.
//! 
//! This crate defines the `Cache` trait used to store and retrieve build events.

use repolith_core::types::ActionId;
use repolith_core::types::BuildEvent;
use async_trait::async_trait;
use thiserror::Error;

/// Errors that can occur during cache operations.
#[derive(Debug, Error)]
pub enum CacheError {
    /// An I/O error occurred while accessing the cache.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A backend-specific error occurred.
    #[error("backend: {0}")]
    Backend(String),
}

/// A specialized `Result` type for cache operations.
pub type Result<T> = std::result::Result<T, CacheError>;

/// A trait for storing and retrieving build events.
#[async_trait]
pub trait Cache: Send + Sync {
    /// Retrieves the last recorded build event for a given action ID.
    async fn last_build(&self, id: &ActionId) -> Option<BuildEvent>;
    
    /// Records a new build event in the cache.
    async fn record(&mut self, event: BuildEvent) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use repolith_core::types::Sha256;

    struct MockCache {
        events: std::collections::HashMap<ActionId, BuildEvent>,
    }

    impl MockCache {
        fn new() -> Self {
            Self {
                events: std::collections::HashMap::new(),
            }
        }
    }

    #[async_trait]
    impl Cache for MockCache {
        async fn last_build(&self, id: &ActionId) -> Option<BuildEvent> {
            self.events.get(id).cloned()
        }

        async fn record(&mut self, event: BuildEvent) -> Result<()> {
            let id = match &event {
                BuildEvent::Success { id, .. } => id.clone(),
                BuildEvent::Failed { id, .. } => id.clone(),
            };
            self.events.insert(id, event);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_mock_cache() {
        let mut cache = MockCache::new();
        let id = ActionId("test-action".to_string());
        
        // Initially empty
        assert!(cache.last_build(&id).await.is_none());

        // Record an event
        let event = BuildEvent::Success {
            id: id.clone(),
            input: Sha256([0; 32]),
            output: Sha256([1; 32]),
            ms: 100,
        };
        
        cache.record(event.clone()).await.unwrap();

        // Retrieve the event
        let retrieved = cache.last_build(&id).await.unwrap();
        match retrieved {
            BuildEvent::Success { ms, .. } => assert_eq!(ms, 100),
            BuildEvent::Failed { .. } => panic!("Expected Success event"),
        }
    }

    #[test]
    fn test_cache_error_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err1 = CacheError::Io(io_err);
        assert_eq!(err1.to_string(), "io: file missing");

        let err2 = CacheError::Backend("db locked".to_string());
        assert_eq!(err2.to_string(), "backend: db locked");
    }

    #[test]
    fn test_cache_is_object_safe() {
        // This line will fail to compile if `Cache` is not object-safe.
        let _cache: Box<dyn Cache> = Box::new(MockCache::new());
    }
}