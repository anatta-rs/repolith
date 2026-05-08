//! Integration tests for [`repolith_engine::orchestrator`].
//!
//! Each test wires up `TestAction`s with a shared [`TestState`] (atomic
//! counters) so we can assert *which* actions ran, when, and how concurrently.

use async_trait::async_trait;
use repolith_cache::{Cache, Result as CacheResult};
use repolith_core::action::Action;
use repolith_core::types::{
    ActionId, BuildError, BuildEvent, BuildOutput, Ctx, ExecMode, Sha256,
};
use repolith_engine::orchestrator::{BuilderError, ExecError, Orchestrator};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Shared test fixtures
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TestState {
    started: AtomicUsize,
    completed: AtomicUsize,
    cancelled: AtomicUsize,
    in_progress: AtomicUsize,
    max_in_progress: AtomicUsize,
}

impl TestState {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn bump_max(&self, current: usize) {
        let mut observed = self.max_in_progress.load(Ordering::SeqCst);
        while current > observed {
            match self.max_in_progress.compare_exchange(
                observed,
                current,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => observed = actual,
            }
        }
    }
}

enum Behavior {
    SucceedAfter(Duration),
    FailImmediately,
}

struct TestAction {
    id: ActionId,
    deps: Vec<ActionId>,
    state: Arc<TestState>,
    behavior: Behavior,
}

impl TestAction {
    fn new(name: &str, deps: &[&str], state: Arc<TestState>, behavior: Behavior) -> Self {
        Self {
            id: ActionId(name.to_string()),
            deps: deps.iter().map(|d| ActionId((*d).to_string())).collect(),
            state,
            behavior,
        }
    }
}

#[async_trait]
impl Action for TestAction {
    fn id(&self) -> ActionId {
        self.id.clone()
    }

    fn deps(&self) -> Vec<ActionId> {
        self.deps.clone()
    }

    async fn input_hash(&self, _ctx: &Ctx) -> Result<Sha256, BuildError> {
        Ok(Sha256([0; 32]))
    }

    async fn execute(&self, ctx: &Ctx) -> Result<BuildOutput, BuildError> {
        self.state.started.fetch_add(1, Ordering::SeqCst);
        let cur = self.state.in_progress.fetch_add(1, Ordering::SeqCst) + 1;
        self.state.bump_max(cur);

        let result = match self.behavior {
            Behavior::FailImmediately => Err(BuildError::UpstreamUnreachable(
                self.id.0.clone(),
            )),
            Behavior::SucceedAfter(d) => {
                tokio::select! {
                    () = tokio::time::sleep(d) => {
                        self.state.completed.fetch_add(1, Ordering::SeqCst);
                        Ok(BuildOutput {
                            output_hash: Sha256([0; 32]),
                            stdout: String::new(),
                        })
                    }
                    () = ctx.cancel.cancelled() => {
                        self.state.cancelled.fetch_add(1, Ordering::SeqCst);
                        Err(BuildError::Cancelled)
                    }
                }
            }
        };

        self.state.in_progress.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

/// Empty in-memory cache that records but always returns `None`, forcing
/// every action to be marked stale (`NoCachedBuild`).
struct AlwaysMissCache {
    stored: std::sync::Mutex<HashMap<ActionId, BuildEvent>>,
}

impl AlwaysMissCache {
    fn new() -> Self {
        Self {
            stored: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Cache for AlwaysMissCache {
    async fn last_build(&self, _id: &ActionId) -> Option<BuildEvent> {
        None
    }

    async fn record(&mut self, event: BuildEvent) -> CacheResult<()> {
        let id = match &event {
            BuildEvent::Success { id, .. } | BuildEvent::Failed { id, .. } => id.clone(),
        };
        self.stored.lock().unwrap().insert(id, event);
        Ok(())
    }
}

/// Cache that returns a Success with matching input hash for every id —
/// makes the plan have empty `reasons()` so `execute_plan` is a no-op.
struct AllHitCache;

#[async_trait]
impl Cache for AllHitCache {
    async fn last_build(&self, id: &ActionId) -> Option<BuildEvent> {
        Some(BuildEvent::Success {
            id: id.clone(),
            input: Sha256([0; 32]),
            output: Sha256([0; 32]),
            ms: 0,
        })
    }

    async fn record(&mut self, _event: BuildEvent) -> CacheResult<()> {
        Ok(())
    }
}

fn ctx() -> Ctx {
    Ctx {
        cancel: CancellationToken::new(),
        workdir: PathBuf::from("/tmp"),
        env: HashMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failfast_cancels() {
    // Layer with A (slow), B (fail immediate), C (slow).
    // FailFast: B's failure must cancel A and C before they complete.
    let state = TestState::new();
    let mut orch = Orchestrator::builder()
        .cache(AlwaysMissCache::new())
        .max_parallelism(8)
        .base_ctx(ctx())
        .register(TestAction::new(
            "A",
            &[],
            state.clone(),
            Behavior::SucceedAfter(Duration::from_millis(500)),
        ))
        .register(TestAction::new(
            "B",
            &[],
            state.clone(),
            Behavior::FailImmediately,
        ))
        .register(TestAction::new(
            "C",
            &[],
            state.clone(),
            Behavior::SucceedAfter(Duration::from_millis(500)),
        ))
        .build()
        .unwrap();

    let plan = orch.compute_plan().await.unwrap();
    let result = orch.execute_plan(&plan, ExecMode::FailFast).await;

    match result {
        Err(ExecError::LayerFailed { events }) => {
            assert_eq!(events.len(), 3, "every action should produce an event");
        }
        other => panic!("expected LayerFailed, got {other:?}"),
    }
    assert_eq!(
        state.cancelled.load(Ordering::SeqCst),
        2,
        "A and C must have observed the cancel before completing"
    );
    assert_eq!(
        state.completed.load(Ordering::SeqCst),
        0,
        "no slow action should run to completion under FailFast"
    );
}

#[tokio::test]
async fn keepgoing_settles() {
    // Same shape as above but mode = KeepGoing.
    // A and C should complete; B fails. execute_plan must still return
    // LayerFailed (we halt before next layer regardless of mode).
    let state = TestState::new();
    let mut orch = Orchestrator::builder()
        .cache(AlwaysMissCache::new())
        .max_parallelism(8)
        .base_ctx(ctx())
        .register(TestAction::new(
            "A",
            &[],
            state.clone(),
            Behavior::SucceedAfter(Duration::from_millis(50)),
        ))
        .register(TestAction::new(
            "B",
            &[],
            state.clone(),
            Behavior::FailImmediately,
        ))
        .register(TestAction::new(
            "C",
            &[],
            state.clone(),
            Behavior::SucceedAfter(Duration::from_millis(50)),
        ))
        .build()
        .unwrap();

    let plan = orch.compute_plan().await.unwrap();
    let result = orch.execute_plan(&plan, ExecMode::KeepGoing).await;

    match result {
        Err(ExecError::LayerFailed { events }) => {
            assert_eq!(events.len(), 3);
        }
        other => panic!("expected LayerFailed, got {other:?}"),
    }
    assert_eq!(
        state.completed.load(Ordering::SeqCst),
        2,
        "A and C must run to completion under KeepGoing"
    );
    assert_eq!(state.cancelled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn semaphore_limits() {
    // 10 concurrent slow actions, max_parallelism = 2 → max_in_progress ≤ 2.
    let state = TestState::new();
    let mut builder = Orchestrator::builder()
        .cache(AlwaysMissCache::new())
        .max_parallelism(2)
        .base_ctx(ctx());
    for i in 0..10 {
        builder = builder.register(TestAction::new(
            &format!("a{i}"),
            &[],
            state.clone(),
            Behavior::SucceedAfter(Duration::from_millis(20)),
        ));
    }
    let mut orch = builder.build().unwrap();

    let plan = orch.compute_plan().await.unwrap();
    let events = orch.execute_plan(&plan, ExecMode::FailFast).await.unwrap();

    assert_eq!(events.len(), 10);
    assert_eq!(state.completed.load(Ordering::SeqCst), 10);
    let observed_max = state.max_in_progress.load(Ordering::SeqCst);
    assert!(
        observed_max <= 2,
        "max_in_progress was {observed_max}, expected ≤ 2"
    );
}

#[tokio::test]
async fn cascade_skip() {
    // Layer 1: A fails. Layer 2: B (depends A) — B must NOT run because the
    // pipeline halts after the failing layer.
    let state = TestState::new();
    let mut orch = Orchestrator::builder()
        .cache(AlwaysMissCache::new())
        .max_parallelism(8)
        .base_ctx(ctx())
        .register(TestAction::new(
            "A",
            &[],
            state.clone(),
            Behavior::FailImmediately,
        ))
        .register(TestAction::new(
            "B",
            &["A"],
            state.clone(),
            Behavior::SucceedAfter(Duration::from_millis(10)),
        ))
        .build()
        .unwrap();

    let plan = orch.compute_plan().await.unwrap();
    assert_eq!(plan.layers().len(), 2, "expected diamond layout");

    let result = orch.execute_plan(&plan, ExecMode::FailFast).await;
    assert!(matches!(result, Err(ExecError::LayerFailed { .. })));
    assert_eq!(
        state.started.load(Ordering::SeqCst),
        1,
        "only A should have started; B's layer must be skipped"
    );
}

#[tokio::test]
async fn empty_stale_returns_immediately() {
    // All actions cached → empty plan reasons → execute_plan is a no-op.
    let state = TestState::new();
    let mut orch = Orchestrator::builder()
        .cache(AllHitCache)
        .max_parallelism(4)
        .base_ctx(ctx())
        .register(TestAction::new(
            "A",
            &[],
            state.clone(),
            Behavior::SucceedAfter(Duration::from_mins(1)),
        ))
        .build()
        .unwrap();

    let plan = orch.compute_plan().await.unwrap();
    assert!(plan.reasons().is_empty(), "AllHitCache → no stale actions");
    let events = orch.execute_plan(&plan, ExecMode::FailFast).await.unwrap();
    assert!(events.is_empty());
    assert_eq!(state.started.load(Ordering::SeqCst), 0);
}

#[test]
fn builder_min_parallelism() {
    // n.max(1) — passing 0 must not produce a 0-permit semaphore.
    let orch = Orchestrator::builder()
        .cache(AllHitCache)
        .max_parallelism(0)
        .build();
    assert!(orch.is_ok(), "build should succeed with max_parallelism=0");
}

#[test]
fn builder_requires_cache() {
    // Forgetting cache must surface a typed error, not panic.
    let result = Orchestrator::builder().build();
    assert!(matches!(result, Err(BuilderError::MissingCache)));
}
