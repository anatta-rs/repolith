//! Integration tests for the [`repolith_core::plan`] module.

use async_trait::async_trait;
use repolith_core::action::Action;
use repolith_core::cache::{Cache, Result as CacheResult};
use repolith_core::plan::{ChangeReason, Plan, PlanError};
use repolith_core::types::{ActionId, BuildError, BuildEvent, BuildOutput, Ctx, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// Minimal in-memory mock cache for plan tests.
struct MockCache {
    events: HashMap<ActionId, BuildEvent>,
}

impl MockCache {
    fn new() -> Self {
        Self {
            events: HashMap::new(),
        }
    }

    fn with_event(mut self, event: BuildEvent) -> Self {
        let id = match &event {
            BuildEvent::Success { id, .. } | BuildEvent::Failed { id, .. } => id.clone(),
        };
        self.events.insert(id, event);
        self
    }
}

#[async_trait]
impl Cache for MockCache {
    async fn last_build(&self, id: &ActionId) -> Option<BuildEvent> {
        self.events.get(id).cloned()
    }

    async fn record(&mut self, event: BuildEvent) -> CacheResult<()> {
        let id = match &event {
            BuildEvent::Success { id, .. } | BuildEvent::Failed { id, .. } => id.clone(),
        };
        self.events.insert(id, event);
        Ok(())
    }
}

/// Stub action whose id, deps, and `input_hash` are all configurable up front.
struct StubAction {
    id: ActionId,
    deps: Vec<ActionId>,
    hash: Sha256,
    /// What `output_present` reports. `true` matches the trait default.
    present: bool,
}

impl StubAction {
    fn new(name: &str, deps: &[&str], hash_byte: u8) -> Self {
        Self {
            id: ActionId(name.to_string()),
            deps: deps.iter().map(|d| ActionId((*d).to_string())).collect(),
            hash: Sha256([hash_byte; 32]),
            present: true,
        }
    }

    /// Simulate an artifact that is gone from this machine — deleted by
    /// hand, or never built here because a peer wrote the cache entry.
    fn without_artifact(mut self) -> Self {
        self.present = false;
        self
    }
}

#[async_trait]
impl Action for StubAction {
    fn id(&self) -> ActionId {
        self.id.clone()
    }

    fn deps(&self) -> Vec<ActionId> {
        self.deps.clone()
    }

    async fn input_hash(&self, _ctx: &Ctx) -> std::result::Result<Sha256, BuildError> {
        Ok(self.hash)
    }

    async fn execute(&self, _ctx: &Ctx) -> std::result::Result<BuildOutput, BuildError> {
        Ok(BuildOutput {
            output_hash: self.hash,
            stdout: String::new(),
        })
    }

    async fn output_present(&self, _ctx: &Ctx) -> bool {
        self.present
    }
}

fn ctx() -> Ctx {
    Ctx {
        cancel: CancellationToken::new(),
        workdir: PathBuf::from("/tmp"),
        env: HashMap::new(),
    }
}

fn aid(s: &str) -> ActionId {
    ActionId(s.to_string())
}

fn boxed(actions: Vec<StubAction>) -> Vec<Box<dyn Action>> {
    actions
        .into_iter()
        .map(|a| Box::new(a) as Box<dyn Action>)
        .collect()
}

#[tokio::test]
async fn test_diamond_layers() {
    // A → {B, C} → D ; expected layers: [[A], [B,C], [D]]
    let actions = boxed(vec![
        StubAction::new("A", &[], 0),
        StubAction::new("B", &["A"], 0),
        StubAction::new("C", &["A"], 0),
        StubAction::new("D", &["B", "C"], 0),
    ]);
    let cache = MockCache::new();
    let plan = Plan::compute(&actions, &cache, &ctx()).await.unwrap();
    let layers = plan.layers();
    assert_eq!(layers.len(), 3, "expected 3 layers, got {layers:?}");
    assert_eq!(layers[0], vec![aid("A")]);
    assert_eq!(layers[1], vec![aid("B"), aid("C")]); // sorted alphabetically
    assert_eq!(layers[2], vec![aid("D")]);
}

#[tokio::test]
async fn test_cycle() {
    // A → B → C → A
    let actions = boxed(vec![
        StubAction::new("A", &["C"], 0),
        StubAction::new("B", &["A"], 0),
        StubAction::new("C", &["B"], 0),
    ]);
    let cache = MockCache::new();
    let err = Plan::compute(&actions, &cache, &ctx()).await.unwrap_err();
    match err {
        PlanError::Cycle(ids) => {
            assert_eq!(ids, vec![aid("A"), aid("B"), aid("C")]);
        }
        other => panic!("expected Cycle, got {other:?}"),
    }
}

#[tokio::test]
async fn test_missing_dep() {
    let actions = boxed(vec![StubAction::new("A", &["GHOST"], 0)]);
    let cache = MockCache::new();
    let err = Plan::compute(&actions, &cache, &ctx()).await.unwrap_err();
    match err {
        PlanError::MissingDep { from, to } => {
            assert_eq!(from, aid("A"));
            assert_eq!(to, aid("GHOST"));
        }
        other => panic!("expected MissingDep, got {other:?}"),
    }
}

#[tokio::test]
async fn test_hash_changed() {
    // Cached input = [0x01;32], current input = [0x02;32]
    let cached = Sha256([0x01; 32]);
    let actions = boxed(vec![StubAction::new("A", &[], 0x02)]);
    let cache = MockCache::new().with_event(BuildEvent::Success {
        id: aid("A"),
        input: cached,
        output: Sha256([0; 32]),
        ms: 1,
    });
    let plan = Plan::compute(&actions, &cache, &ctx()).await.unwrap();
    match plan.reasons().get(&aid("A")) {
        Some(ChangeReason::InputHashChanged { from, to }) => {
            assert_eq!(*from, cached);
            assert_eq!(*to, Sha256([0x02; 32]));
        }
        other => panic!("expected InputHashChanged, got {other:?}"),
    }
}

#[tokio::test]
async fn test_cascade_upstream_moved() {
    // A: no cached build → stale (NoCachedBuild)
    // B depends on A, B's hash unchanged vs cache → UpstreamMoved{A}
    let b_hash = Sha256([0x42; 32]);
    let actions = boxed(vec![
        StubAction::new("A", &[], 0x01),
        StubAction::new("B", &["A"], 0x42),
    ]);
    // Cache has B's prior build with matching input hash, but no entry for A.
    let cache = MockCache::new().with_event(BuildEvent::Success {
        id: aid("B"),
        input: b_hash,
        output: Sha256([0; 32]),
        ms: 1,
    });
    let plan = Plan::compute(&actions, &cache, &ctx()).await.unwrap();
    assert_eq!(
        plan.reasons().get(&aid("A")),
        Some(&ChangeReason::NoCachedBuild)
    );
    match plan.reasons().get(&aid("B")) {
        Some(ChangeReason::UpstreamMoved { dep }) => assert_eq!(*dep, aid("A")),
        other => panic!("expected UpstreamMoved, got {other:?}"),
    }
}

#[tokio::test]
async fn test_failed_prior_rebuilds() {
    // Cached BuildEvent::Failed → must always re-run. Since #86 the reason
    // names the failure instead of pretending the action never ran, but the
    // planning decision — stale, re-run — is unchanged.
    let actions = boxed(vec![StubAction::new("A", &[], 0x01)]);
    let cache = MockCache::new().with_event(BuildEvent::Failed {
        id: aid("A"),
        input: Sha256([0x01; 32]),
        error: BuildError::Cancelled,
        ms: 1,
    });
    let plan = Plan::compute(&actions, &cache, &ctx()).await.unwrap();
    assert!(
        matches!(
            plan.reasons().get(&aid("A")),
            Some(ChangeReason::PreviousFailure { .. })
        ),
        "got {:?}",
        plan.reasons().get(&aid("A"))
    );
    assert_eq!(plan.reasons().len(), 1, "still stale, still re-runs");
}

#[tokio::test]
async fn test_empty_actions() {
    let actions: Vec<Box<dyn Action>> = vec![];
    let cache = MockCache::new();
    let plan = Plan::compute(&actions, &cache, &ctx()).await.unwrap();
    assert!(plan.layers().is_empty());
    assert!(plan.reasons().is_empty());
    assert_eq!(plan.stale().count(), 0);
}

#[tokio::test]
async fn test_cancel_during_compute_returns_cancelled() {
    // Two-layer plan; pre-cancel the token so `Plan::compute` should
    // bail at the top of the second layer iteration with
    // `PlanError::Build(BuildError::Cancelled)`.
    let actions = boxed(vec![
        StubAction::new("A", &[], 0),
        StubAction::new("B", &["A"], 0),
    ]);
    let cache = MockCache::new();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = Ctx {
        cancel,
        workdir: PathBuf::from("/tmp"),
        env: HashMap::new(),
    };
    let err = Plan::compute(&actions, &cache, &ctx).await.unwrap_err();
    match err {
        PlanError::Build(BuildError::Cancelled) => (),
        other => panic!("expected Build(Cancelled), got {other:?}"),
    }
}

/// Stub action whose `input_hash` sleeps for a configurable duration.
/// Used to verify cancel interrupts a layer's in-flight fan-out.
struct SlowAction {
    id: ActionId,
    delay: std::time::Duration,
}

#[async_trait]
impl Action for SlowAction {
    fn id(&self) -> ActionId {
        self.id.clone()
    }
    fn deps(&self) -> Vec<ActionId> {
        Vec::new()
    }
    async fn input_hash(&self, _ctx: &Ctx) -> std::result::Result<Sha256, BuildError> {
        tokio::time::sleep(self.delay).await;
        Ok(Sha256([0; 32]))
    }
    async fn execute(&self, _ctx: &Ctx) -> std::result::Result<BuildOutput, BuildError> {
        Ok(BuildOutput {
            output_hash: Sha256([0; 32]),
            stdout: String::new(),
        })
    }
}

#[tokio::test]
async fn test_cancel_during_layer_fanout_short_circuits() {
    // Layer of 4 slow actions, each sleeping 10s in `input_hash`. Fire
    // cancel 100ms after compute starts. Without the inner select! against
    // ctx.cancel the compute would wait the full 10s. With it, the futures
    // observe the cancel and return Cancelled in ~the cancel delay.
    let actions: Vec<Box<dyn Action>> = (0..4)
        .map(|i| {
            Box::new(SlowAction {
                id: aid(&format!("slow-{i}")),
                delay: std::time::Duration::from_secs(10),
            }) as Box<dyn Action>
        })
        .collect();
    let cache = MockCache::new();
    let cancel = CancellationToken::new();
    let ctx = Ctx {
        cancel: cancel.clone(),
        workdir: PathBuf::from("/tmp"),
        env: HashMap::new(),
    };
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let start = std::time::Instant::now();
    let result = Plan::compute(&actions, &cache, &ctx).await;
    let elapsed = start.elapsed();
    canceller.await.unwrap();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "compute should short-circuit on cancel; took {elapsed:?}"
    );
    match result {
        Err(PlanError::Build(BuildError::Cancelled)) => (),
        other => panic!("expected Build(Cancelled), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// OutputMissing — a cached Success is not proof the artifact is still here
// (issue #73)
// ---------------------------------------------------------------------------

/// The shared-cache regression: machine A records a Success, machine B has
/// the same manifest and the same (shared) cache but no artifact. Before
/// this, B reported `0 stale actions` and installed nothing at all.
#[tokio::test]
async fn artifact_missing_is_stale_even_when_the_hash_matches() {
    let action = StubAction::new("a", &[], 1).without_artifact();
    let hash = Sha256([1; 32]);
    let cache = MockCache::new().with_event(BuildEvent::Success {
        id: aid("a"),
        input: hash,
        output: hash,
        ms: 1,
    });

    let plan = Plan::compute(&boxed(vec![action]), &cache, &ctx())
        .await
        .expect("plan");

    assert_eq!(
        plan.reasons().get(&aid("a")),
        Some(&ChangeReason::OutputMissing),
        "a cached Success with no artifact on this machine must be stale"
    );
}

/// Same inputs, artifact present: still a no-op. Guards against the fix
/// turning every sync into a rebuild.
#[tokio::test]
async fn artifact_present_with_matching_hash_stays_up_to_date() {
    let hash = Sha256([1; 32]);
    let cache = MockCache::new().with_event(BuildEvent::Success {
        id: aid("a"),
        input: hash,
        output: hash,
        ms: 1,
    });

    let plan = Plan::compute(&boxed(vec![StubAction::new("a", &[], 1)]), &cache, &ctx())
        .await
        .expect("plan");

    assert!(
        plan.reasons().is_empty(),
        "unchanged inputs + present artifact must stay up to date, got {:?}",
        plan.reasons()
    );
}

/// A changed hash still wins over the presence probe — the reason must
/// stay the more informative `InputHashChanged`.
#[tokio::test]
async fn changed_hash_outranks_missing_artifact() {
    let cache = MockCache::new().with_event(BuildEvent::Success {
        id: aid("a"),
        input: Sha256([9; 32]),
        output: Sha256([9; 32]),
        ms: 1,
    });

    let plan = Plan::compute(
        &boxed(vec![StubAction::new("a", &[], 1).without_artifact()]),
        &cache,
        &ctx(),
    )
    .await
    .expect("plan");

    assert!(
        matches!(
            plan.reasons().get(&aid("a")),
            Some(ChangeReason::InputHashChanged { .. })
        ),
        "got {:?}",
        plan.reasons().get(&aid("a"))
    );
}

// ---------------------------------------------------------------------------
// PreviousFailure + Display — status must say what went wrong (issue #86)
// ---------------------------------------------------------------------------

/// A recorded failure must be reported as such, **and** still re-run. The
/// label changes; the planning decision does not.
#[tokio::test]
async fn prior_failure_is_named_and_still_stale() {
    let cache = MockCache::new().with_event(BuildEvent::Failed {
        id: aid("a"),
        input: Sha256([1; 32]),
        error: BuildError::CommandFailed {
            exit_code: 101,
            stderr: "error: does not contain a Cargo.toml file".to_string(),
        },
        ms: 5,
    });

    let plan = Plan::compute(&boxed(vec![StubAction::new("a", &[], 1)]), &cache, &ctx())
        .await
        .expect("plan");

    match plan.reasons().get(&aid("a")) {
        Some(ChangeReason::PreviousFailure { error }) => {
            assert!(
                error.contains("Cargo.toml"),
                "the recorded stderr must survive, got: {error}"
            );
        }
        other => panic!("expected PreviousFailure, got {other:?}"),
    }
    assert_eq!(plan.reasons().len(), 1, "and it must still be stale");
}

/// No cache entry is a different situation from a recorded failure.
#[tokio::test]
async fn absent_entry_is_still_no_cached_build() {
    let plan = Plan::compute(
        &boxed(vec![StubAction::new("a", &[], 1)]),
        &MockCache::new(),
        &ctx(),
    )
    .await
    .expect("plan");

    assert_eq!(
        plan.reasons().get(&aid("a")),
        Some(&ChangeReason::NoCachedBuild)
    );
}

#[test]
fn display_is_short_and_readable() {
    // The bug: Debug dumped a 32-byte array per hash and blew the table to
    // ~400 columns. Display must stay table-sized.
    let changed = ChangeReason::InputHashChanged {
        from: Sha256([0; 32]),
        to: Sha256([255; 32]),
    };
    let s = changed.to_string();
    assert_eq!(s, "inputs changed (00000000 -> ffffffff)");
    assert!(s.len() < 60, "must fit a table cell, got {} chars", s.len());

    assert_eq!(ChangeReason::NoCachedBuild.to_string(), "never built");
    assert_eq!(ChangeReason::OutputMissing.to_string(), "artifact missing");
    assert_eq!(
        ChangeReason::UpstreamMoved {
            dep: aid("dep::x::0")
        }
        .to_string(),
        "upstream dep::x::0 is stale"
    );
}

#[test]
fn multiline_failure_is_truncated_to_one_line() {
    let reason = ChangeReason::PreviousFailure {
        error: "error: the real message\n  --> context line\n  note: more".to_string(),
    };
    let s = reason.to_string();
    assert!(s.contains("the real message"), "keeps the first line: {s}");
    assert!(!s.contains("context line"), "drops the rest: {s}");
    assert!(!s.contains('\n'), "single line for a table cell: {s}");
}

#[test]
fn very_long_failure_is_capped() {
    let reason = ChangeReason::PreviousFailure {
        error: "x".repeat(300),
    };
    let s = reason.to_string();
    assert!(s.len() < 110, "capped, got {} chars", s.len());
    assert!(s.ends_with("..."), "signals truncation: {s}");
}
