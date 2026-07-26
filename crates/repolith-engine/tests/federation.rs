//! Integration tests for `repolith_engine::federation` — nested stacks,
//! cycle detection, depth bound, pre-clone hashing, and the coordinator
//! semaphore exemption (no deadlock at --jobs 1).

use async_trait::async_trait;
use repolith_cache::SqliteCache;
use repolith_core::action::Action;
use repolith_core::cache::Cache;
use repolith_core::manifest::Manifest;
use repolith_core::types::{ActionId, BuildError, BuildOutput, Ctx, ExecMode, Sha256};
use repolith_engine::federation::{FederationHooks, MAX_FEDERATION_DEPTH, RepolithSync};
use repolith_engine::orchestrator::Orchestrator;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const CHILD_MANIFEST: &str = "
[orchestrator]
schema_version = \"0.1\"
name = \"child\"
";

/// Stub action that bumps a counter on execute — stands in for the child
/// stack's real work. The **child orchestrator** charges it a permit from
/// the shared semaphore, which is exactly what the deadlock test relies on.
struct CountingAction {
    id: ActionId,
    counter: Arc<AtomicUsize>,
}

#[async_trait]
impl Action for CountingAction {
    fn id(&self) -> ActionId {
        self.id.clone()
    }
    fn deps(&self) -> Vec<ActionId> {
        vec![]
    }
    async fn input_hash(&self, _ctx: &Ctx) -> Result<Sha256, BuildError> {
        Ok(Sha256([7; 32]))
    }
    async fn execute(&self, _ctx: &Ctx) -> Result<BuildOutput, BuildError> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(BuildOutput {
            output_hash: Sha256([8; 32]),
            stdout: String::new(),
        })
    }
}

/// Hooks that hand every child stack one `CountingAction` and an
/// in-memory cache.
struct StubHooks {
    counter: Arc<AtomicUsize>,
}

impl FederationHooks for StubHooks {
    fn build_actions(
        &self,
        _manifest: &Manifest,
        _stack_dir: &Path,
        _chain: &[PathBuf],
    ) -> Result<Vec<Box<dyn Action>>, BuildError> {
        Ok(vec![Box::new(CountingAction {
            id: ActionId("child::work::0".into()),
            counter: Arc::clone(&self.counter),
        })])
    }

    fn open_cache(&self, _stack_dir: &Path) -> Result<Box<dyn Cache>, BuildError> {
        let cache = SqliteCache::in_memory()
            .map_err(|e| BuildError::Io(format!("in-memory cache: {e}")))?;
        Ok(Box::new(cache))
    }
}

fn ctx() -> Ctx {
    Ctx {
        cancel: CancellationToken::new(),
        workdir: PathBuf::from("."),
        env: std::collections::HashMap::new(),
    }
}

fn sync_action(
    dir: &Path,
    chain: Vec<PathBuf>,
    counter: &Arc<AtomicUsize>,
    sem: Arc<Semaphore>,
) -> RepolithSync {
    RepolithSync {
        id: ActionId("stack::repolith::0".into()),
        node_path: dir.to_path_buf(),
        manifest_rel: None,
        chain,
        mode: ExecMode::FailFast,
        shared_sem: sem,
        hooks: Arc::new(StubHooks {
            counter: Arc::clone(counter),
        }),
        deps: vec![],
    }
}

#[tokio::test]
async fn nested_stack_executes_child_actions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("repolith.toml"), CHILD_MANIFEST).unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let action = sync_action(dir.path(), vec![], &counter, Arc::new(Semaphore::new(4)));

    let out = action.execute(&ctx()).await.expect("child sync succeeds");
    assert_eq!(counter.load(Ordering::SeqCst), 1, "child action ran once");
    assert!(out.stdout.contains("1 action(s)"), "stdout: {}", out.stdout);
}

#[tokio::test]
async fn cycle_is_rejected_with_chain() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("repolith.toml");
    std::fs::write(&manifest, CHILD_MANIFEST).unwrap();
    let canon = manifest.canonicalize().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    // The child manifest is already in the ancestor chain -> A -> ... -> A.
    let action = sync_action(
        dir.path(),
        vec![canon],
        &counter,
        Arc::new(Semaphore::new(4)),
    );

    let err = action.execute(&ctx()).await.unwrap_err();
    assert!(err.to_string().contains("federation cycle"), "got: {err}");
    assert_eq!(counter.load(Ordering::SeqCst), 0, "no child action ran");
}

#[tokio::test]
async fn depth_bound_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("repolith.toml"), CHILD_MANIFEST).unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let chain: Vec<PathBuf> = (0..MAX_FEDERATION_DEPTH)
        .map(|i| PathBuf::from(format!("/ancestor/{i}/repolith.toml")))
        .collect();
    let action = sync_action(dir.path(), chain, &counter, Arc::new(Semaphore::new(4)));

    let err = action.execute(&ctx()).await.unwrap_err();
    assert!(err.to_string().contains("federation depth"), "got: {err}");
}

#[tokio::test]
async fn input_hash_pre_clone_is_deterministic_marker() {
    let counter = Arc::new(AtomicUsize::new(0));
    let missing = PathBuf::from("/nonexistent/checkout/for/repolith/tests");
    let action = sync_action(&missing, vec![], &counter, Arc::new(Semaphore::new(4)));

    let h1 = action.input_hash(&ctx()).await.expect("marker hash");
    let h2 = action.input_hash(&ctx()).await.expect("marker hash again");
    assert_eq!(h1, h2, "pre-clone marker must be deterministic");
}

/// The deadlock regression test: a coordinator inside a --jobs 1 parent
/// orchestrator whose child action draws from the SAME shared semaphore.
/// Without the coordinator exemption this hangs (parent holds the only
/// permit while the child waits for it).
#[tokio::test]
async fn no_deadlock_at_jobs_1_with_shared_semaphore() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("repolith.toml"), CHILD_MANIFEST).unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let sem = Arc::new(Semaphore::new(1));
    let action = sync_action(dir.path(), vec![], &counter, Arc::clone(&sem));

    let mut orch = Orchestrator::builder()
        .cache(SqliteCache::in_memory().unwrap())
        .shared_semaphore(sem)
        .base_ctx(ctx())
        .register(action)
        .build()
        .unwrap();

    let plan = orch.compute_plan().await.expect("plan");
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.execute_plan(&plan, ExecMode::FailFast),
    )
    .await
    .expect("must not deadlock")
    .expect("child sync succeeds");

    assert_eq!(events.len(), 1);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}
