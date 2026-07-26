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

    /// Whether the artifact this action produces is still present.
    ///
    /// A cached `Success` only means "these inputs were built **somewhere,
    /// once**" — not that the result is still on *this* machine. Without
    /// this probe the planner trusts the cache blindly and reports
    /// `up to date` when the binary was deleted, or when the cache entry
    /// was written by a different machine sharing a backend (see the
    /// Neo4j backend). Actions that leave a persistent artifact should
    /// override this; the default `true` suits actions with nothing to
    /// check (coordinators, pure side effects).
    ///
    /// Implementations must be **cheap** — this runs for every action on
    /// every plan. A `stat` or a single subprocess, nothing more. On any
    /// doubt return `true`: a false negative causes a needless rebuild,
    /// while a false positive resurrects the silent-staleness bug.
    async fn output_present(&self, ctx: &Ctx) -> bool {
        let _ = ctx;
        true
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
