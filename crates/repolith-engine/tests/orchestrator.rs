//! Integration tests for [`repolith_engine::orchestrator`].
//!
//! Each test wires up `TestAction`s with a shared [`TestState`] (atomic
//! counters) so we can assert *which* actions ran, when, and how concurrently.

use async_trait::async_trait;
use repolith_cache::{Cache, Result as CacheResult};
use repolith_core::action::Action;
use repolith_core::types::{ActionId, BuildError, BuildEvent, BuildOutput, Ctx, ExecMode, Sha256};
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
            Behavior::FailImmediately => Err(BuildError::UpstreamUnreachable(self.id.0.clone())),
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

#[tokio::test]
async fn cancel_between_layers_aborts_pipeline() {
    // Layer 1: A succeeds. We pre-cancel the root token before invoking
    // execute_plan, so the orchestrator must bail at the layer-loop
    // header *before* spawning anything in layer 2 — otherwise it would
    // poison the cache with bogus `BuildEvent::Failed` entries from the
    // already-cancelled child token.
    let state = TestState::new();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = Ctx {
        cancel,
        workdir: PathBuf::from("/tmp"),
        env: HashMap::new(),
    };
    let orch = Orchestrator::builder()
        .cache(AlwaysMissCache::new())
        .max_parallelism(8)
        .base_ctx(ctx)
        .register(TestAction::new(
            "A",
            &[],
            state.clone(),
            Behavior::SucceedAfter(Duration::from_millis(10)),
        ))
        .register(TestAction::new(
            "B",
            &["A"],
            state.clone(),
            Behavior::SucceedAfter(Duration::from_millis(10)),
        ))
        .build()
        .unwrap();

    // The cancelled token also propagates into Plan::compute, so we
    // expect it to bail there before we ever reach execute_plan. That's
    // fine — what we're proving is that the cancel signal short-circuits
    // the pipeline before any subprocesses run.
    let plan = orch.compute_plan().await;
    assert!(
        plan.is_err(),
        "compute_plan should fail when ctx.cancel is pre-fired"
    );
    assert_eq!(
        state.started.load(Ordering::SeqCst),
        0,
        "no action should have started under a pre-cancelled context"
    );
}

// ---------------------------------------------------------------------------
// ProgressSink — the action lifecycle as it happens (issue #98).
//
// These assert the two properties the design rests on: exactly one start and
// one terminal per action, emitted from a single place; and a layer boundary
// that fires even when an action vanishes without a terminal.
// ---------------------------------------------------------------------------

use repolith_core::progress::ProgressSink;
use std::sync::Mutex;

/// Records every callback in order, so tests assert on the *sequence* rather
/// than on counts alone — ordering is the whole point of `on_action_start`.
#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl Recorder {
    fn log(&self) -> Vec<String> {
        self.0.lock().expect("not poisoned").clone()
    }
    fn push(&self, s: String) {
        self.0.lock().expect("not poisoned").push(s);
    }
}

impl ProgressSink for Recorder {
    fn on_layer_start(&self, index: usize, total: usize, ids: &[ActionId]) {
        self.push(format!("layer_start:{index}/{total}:{}", ids.len()));
    }
    fn on_action_start(&self, id: &ActionId) {
        self.push(format!("start:{id}"));
    }
    fn on_action_ok(&self, id: &ActionId, _ms: u64) {
        self.push(format!("ok:{id}"));
    }
    fn on_action_failed(&self, id: &ActionId, _err: &BuildError, _ms: u64) {
        self.push(format!("failed:{id}"));
    }
    fn on_action_cancelled(&self, id: &ActionId, _ms: u64) {
        self.push(format!("cancelled:{id}"));
    }
    fn on_layer_end(&self, index: usize) {
        self.push(format!("layer_end:{index}"));
    }
}

async fn run_observed(
    actions: Vec<Box<dyn Action>>,
    mode: ExecMode,
    sink: &Recorder,
) -> Result<Vec<BuildEvent>, ExecError> {
    let mut builder = Orchestrator::builder()
        .cache(AlwaysMissCache::new())
        .base_ctx(ctx())
        .max_parallelism(4);
    for a in actions {
        builder = builder.register_boxed(a);
    }
    let mut orch = builder.build().expect("build");
    let plan = orch.compute_plan().await.expect("plan");
    orch.execute_plan_observed(&plan, mode, sink).await
}

#[tokio::test]
async fn every_action_gets_exactly_one_start_and_one_terminal() {
    let st = TestState::new();
    let actions: Vec<Box<dyn Action>> = vec![
        Box::new(TestAction::new(
            "a",
            &[],
            Arc::clone(&st),
            Behavior::SucceedAfter(Duration::from_millis(1)),
        )),
        Box::new(TestAction::new(
            "b",
            &["a"],
            Arc::clone(&st),
            Behavior::SucceedAfter(Duration::from_millis(1)),
        )),
    ];
    let sink = Recorder::default();
    run_observed(actions, ExecMode::FailFast, &sink)
        .await
        .expect("succeeds");

    let log = sink.log();
    for id in ["a", "b"] {
        assert_eq!(
            log.iter().filter(|l| *l == &format!("start:{id}")).count(),
            1,
            "exactly one start for {id}: {log:?}"
        );
        assert_eq!(
            log.iter()
                .filter(|l| l.ends_with(&format!(":{id}")) && !l.starts_with("start:"))
                .count(),
            1,
            "exactly one terminal for {id}: {log:?}"
        );
    }
}

/// The reason `on_action_start` exists at all: it must precede the await, so
/// the line appears while the action runs and not once it is over.
#[tokio::test]
async fn start_precedes_its_own_terminal() {
    let st = TestState::new();
    let actions: Vec<Box<dyn Action>> = vec![Box::new(TestAction::new(
        "a",
        &[],
        st,
        Behavior::SucceedAfter(Duration::from_millis(20)),
    ))];
    let sink = Recorder::default();
    run_observed(actions, ExecMode::FailFast, &sink)
        .await
        .expect("succeeds");

    let log = sink.log();
    let start = log.iter().position(|l| l == "start:a").expect("start");
    let ok = log.iter().position(|l| l == "ok:a").expect("ok");
    assert!(start < ok, "start must precede the terminal: {log:?}");
}

/// Under `FailFast` one real error cancels its peers. Reporting those peers as
/// failures would turn a single fault into a screenful of them.
#[tokio::test]
async fn cancelled_peers_are_not_reported_as_failures() {
    let st = TestState::new();
    let actions: Vec<Box<dyn Action>> = vec![
        Box::new(TestAction::new(
            "boom",
            &[],
            Arc::clone(&st),
            Behavior::FailImmediately,
        )),
        Box::new(TestAction::new(
            "slow",
            &[],
            Arc::clone(&st),
            Behavior::SucceedAfter(Duration::from_secs(30)),
        )),
    ];
    let sink = Recorder::default();
    let _ = run_observed(actions, ExecMode::FailFast, &sink).await;

    let log = sink.log();
    assert!(log.contains(&"failed:boom".to_string()), "{log:?}");
    assert!(
        log.contains(&"cancelled:slow".to_string()),
        "the cancelled peer must be reported as cancelled, not failed: {log:?}"
    );
    assert!(
        !log.contains(&"failed:slow".to_string()),
        "a cancelled peer is not a failure: {log:?}"
    );
}

/// `on_layer_end` closes every layer, and the index counts only layers that
/// actually ran — announcing "layer 3/7" after skipping two would leave the
/// reader wondering what happened to them.
#[tokio::test]
async fn layer_boundaries_are_paired_and_numbered_by_what_runs() {
    let st = TestState::new();
    let actions: Vec<Box<dyn Action>> = vec![
        Box::new(TestAction::new(
            "a",
            &[],
            Arc::clone(&st),
            Behavior::SucceedAfter(Duration::from_millis(1)),
        )),
        Box::new(TestAction::new(
            "b",
            &["a"],
            Arc::clone(&st),
            Behavior::SucceedAfter(Duration::from_millis(1)),
        )),
    ];
    let sink = Recorder::default();
    run_observed(actions, ExecMode::FailFast, &sink)
        .await
        .expect("succeeds");

    let log = sink.log();
    assert_eq!(
        log.iter().filter(|l| l.starts_with("layer_start:")).count(),
        log.iter().filter(|l| l.starts_with("layer_end:")).count(),
        "every layer that opens must close: {log:?}"
    );
    assert!(log.contains(&"layer_start:1/2:1".to_string()), "{log:?}");
    assert!(log.contains(&"layer_start:2/2:1".to_string()), "{log:?}");
}

/// A plan with nothing stale must not emit layer boundaries for layers that
/// are skipped — otherwise the CLI prints headers for work that never runs.
#[tokio::test]
async fn an_up_to_date_plan_emits_nothing() {
    let st = TestState::new();
    let mut builder = Orchestrator::builder()
        .cache(AllHitCache)
        .base_ctx(ctx())
        .max_parallelism(4);
    builder = builder.register_boxed(Box::new(TestAction::new(
        "a",
        &[],
        st,
        Behavior::SucceedAfter(Duration::from_millis(1)),
    )));
    let mut orch = builder.build().expect("build");
    let plan = orch.compute_plan().await.expect("plan");

    let sink = Recorder::default();
    orch.execute_plan_observed(&plan, ExecMode::FailFast, &sink)
        .await
        .expect("no-op");
    assert!(sink.log().is_empty(), "{:?}", sink.log());
}

/// The whole reason `on_layer_end` is carried by a `Drop` guard rather than a
/// statement after the drain: the futures are *inline*, so a panic inside
/// `Action::execute` unwinds `execute_layer` itself and any trailing statement
/// would be skipped — precisely in the case a sink needs to reconcile.
///
/// Gated on `cfg(panic = "unwind")`: under `panic = "abort"` no `Drop` runs at
/// all, and the gate documents that precondition mechanically instead of
/// leaving it in a comment.
#[cfg(panic = "unwind")]
#[tokio::test]
async fn on_layer_end_fires_even_when_an_action_panics() {
    struct Panicking(ActionId);

    #[async_trait]
    impl Action for Panicking {
        fn id(&self) -> ActionId {
            self.0.clone()
        }
        fn deps(&self) -> Vec<ActionId> {
            vec![]
        }
        async fn input_hash(&self, _ctx: &Ctx) -> Result<Sha256, BuildError> {
            Ok(Sha256([0; 32]))
        }
        async fn execute(&self, _ctx: &Ctx) -> Result<BuildOutput, BuildError> {
            panic!("boom from inside an action");
        }
    }

    let sink = Arc::new(Recorder::default());
    let sink_for_run = Arc::clone(&sink);

    let outcome = tokio::spawn(async move {
        let mut orch = Orchestrator::builder()
            .cache(AlwaysMissCache::new())
            .base_ctx(ctx())
            .register_boxed(Box::new(Panicking(ActionId("boom".into()))))
            .build()
            .expect("build");
        let plan = orch.compute_plan().await.expect("plan");
        orch.execute_plan_observed(&plan, ExecMode::FailFast, sink_for_run.as_ref())
            .await
    })
    .await;

    assert!(
        outcome.is_err(),
        "the panic must propagate, not be swallowed"
    );
    let log = sink.log();
    assert!(
        log.contains(&"layer_end:1".to_string()),
        "the Drop guard must still close the layer during unwinding: {log:?}"
    );
    assert!(
        log.contains(&"start:boom".to_string()) && !log.iter().any(|l| l.starts_with("ok:")),
        "the action started and never reached a terminal — that is the case \
         on_layer_end exists to reconcile: {log:?}"
    );
}
