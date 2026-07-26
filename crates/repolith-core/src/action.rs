use crate::types::{ActionId, BuildError, BuildOutput, Ctx, Sha256};
use async_trait::async_trait;

/// Represents an executable action within the repolith graph.
///
/// Actions are the fundamental units of work. They declare their dependencies,
/// compute their input state, and execute their logic.
///
/// Note: Long-running `execute` implementations should periodically poll
/// `ctx.cancel.is_cancelled()` to support graceful termination.
#[async_trait]
pub trait Action: Send + Sync {
    /// Returns the unique identifier for this action.
    fn id(&self) -> ActionId;

    /// Returns the list of action IDs that this action depends on.
    fn deps(&self) -> Vec<ActionId>;

    /// Computes the hash representing the input state of this action.
    /// This is used for caching and determining if the action needs to run.
    async fn input_hash(&self, ctx: &Ctx) -> Result<Sha256, BuildError>;

    /// Executes the action's core logic.
    async fn execute(&self, ctx: &Ctx) -> Result<BuildOutput, BuildError>;

    /// `true` for actions that only *coordinate* other actions (federation's
    /// `RepolithSync`) rather than doing bounded work themselves.
    ///
    /// The orchestrator does **not** charge a concurrency permit for
    /// coordinators: the child actions they spawn acquire from the same
    /// shared semaphore, so charging the coordinator too would deadlock at
    /// `--jobs 1` (the coordinator would hold the only permit while its
    /// children wait for it).
    fn is_coordinator(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockAction;

    #[async_trait]
    impl Action for MockAction {
        fn id(&self) -> ActionId {
            ActionId("mock".to_string())
        }

        fn deps(&self) -> Vec<ActionId> {
            vec![]
        }

        async fn input_hash(&self, _ctx: &Ctx) -> Result<Sha256, BuildError> {
            Ok(Sha256([0; 32]))
        }

        async fn execute(&self, _ctx: &Ctx) -> Result<BuildOutput, BuildError> {
            Ok(BuildOutput {
                output_hash: Sha256([1; 32]),
                stdout: String::new(),
            })
        }
    }

    #[test]
    fn test_action_is_object_safe() {
        // This line will fail to compile if `Action` is not object-safe.
        let _action: Box<dyn Action> = Box::new(MockAction);
    }
}
