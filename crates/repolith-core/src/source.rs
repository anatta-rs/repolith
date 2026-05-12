use crate::types::{BuildError, Sha256};
use async_trait::async_trait;

/// Represents a source of code or data that can be fetched.
///
/// A source is typically a remote repository (like Git) that needs to be
/// synchronized before actions can run.
#[async_trait]
pub trait Source: Send + Sync {
    /// Returns the current hash (e.g., commit SHA) of the source.
    async fn current_sha(&self) -> Result<Sha256, BuildError>;

    /// Fetches the latest data from the source.
    async fn fetch(&mut self) -> Result<(), BuildError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockSource;

    #[async_trait]
    impl Source for MockSource {
        async fn current_sha(&self) -> Result<Sha256, BuildError> {
            Ok(Sha256([0; 32]))
        }

        async fn fetch(&mut self) -> Result<(), BuildError> {
            Ok(())
        }
    }

    #[test]
    fn test_source_is_object_safe() {
        // This line will fail to compile if `Source` is not object-safe.
        let _source: Box<dyn Source> = Box::new(MockSource);
    }
}
