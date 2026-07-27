//! Build plan — immutable DAG snapshot with staleness reasons.
//!
//! `Plan::compute` traverses the action graph topologically (Kahn layers)
//! and decides which actions are stale by comparing their current input hash
//! against the last recorded `BuildEvent` in the [cache](crate::cache::Cache).
//! Stale-ness cascades: an action whose hash didn't change but which depends
//! on a stale ancestor is marked `ChangeReason::UpstreamMoved`.
//!
//! The resulting `Plan` is owned, cloneable, and can be replayed any number
//! of times by the orchestrator (with `--dry-run`, `--explain`, etc.).

use crate::action::Action;
use crate::cache::Cache;
use crate::types::{ActionId, BuildError, BuildEvent, Ctx, Sha256};
use futures::future::{Either, join_all, select};
use std::collections::{HashMap, HashSet};
use std::pin::pin;
use thiserror::Error;

/// Immutable, topologically-layered execution plan.
///
/// Layer `n` holds only actions whose dependencies are all in layers `0..n`,
/// so layers can be executed in order; actions within a single layer are
/// independent and may run in parallel.
#[derive(Clone, Debug)]
pub struct Plan {
    layers: Vec<Vec<ActionId>>,
    reasons: HashMap<ActionId, ChangeReason>,
    /// Input hash captured at `Plan::compute` time, keyed by action id.
    /// Stored so the orchestrator can reuse it without re-running every
    /// action's `input_hash` (which may shell out to git/cargo).
    input_hashes: HashMap<ActionId, Sha256>,
}

/// Why a given action is considered stale and needs to re-run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeReason {
    /// The cache has no prior build event for this action.
    NoCachedBuild,
    /// The action was previously built, but its current input hash differs.
    InputHashChanged {
        /// Hash recorded in the last successful build.
        from: Sha256,
        /// Hash computed for the current run.
        to: Sha256,
    },
    /// The action's own hash is unchanged, but a transitive dependency is stale.
    UpstreamMoved {
        /// First stale dependency encountered (deterministic via deps order).
        dep: ActionId,
    },
    /// Inputs are unchanged and the cache holds a `Success`, but the
    /// artifact it describes is gone from this machine — deleted by hand,
    /// or never here in the first place because another machine wrote the
    /// entry into a shared cache backend.
    OutputMissing,
    /// The last recorded run of this action failed. Carries the rendered
    /// error so `status` can say what went wrong instead of pretending the
    /// action was never run.
    ///
    /// Holds a `String` rather than a [`BuildError`]:
    /// that type is deliberately not `PartialEq`, which this enum needs.
    PreviousFailure {
        /// `BuildError`'s `Display` output, as recorded in the cache.
        error: String,
    },
    /// The caller asked for this action to run regardless of cache state
    /// (`repolith sync --force`).
    ///
    /// Distinct from [`Self::NoCachedBuild`] on purpose: "you told me to"
    /// and "I have never seen this" lead to the same work but mean very
    /// different things when reading `--explain` output, and conflating
    /// them would make a forced run indistinguishable from a lost cache.
    Forced,
}

impl std::fmt::Display for ChangeReason {
    /// One short line per reason, suitable for a table cell. `Debug` stays
    /// derived for tests and for anyone who wants the full hashes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCachedBuild => write!(f, "never built"),
            Self::Forced => write!(f, "forced"),
            Self::InputHashChanged { from, to } => {
                write!(f, "inputs changed ({} -> {})", from.short(), to.short())
            }
            Self::UpstreamMoved { dep } => write!(f, "upstream {dep} is stale"),
            Self::OutputMissing => write!(f, "artifact missing"),
            Self::PreviousFailure { error } => {
                // Cargo and git put the message on the first line and the
                // context below; a table cell only has room for the former.
                let first = error.lines().next().unwrap_or(error);
                let trimmed: String = first.chars().take(72).collect();
                if trimmed.len() < first.len() {
                    write!(f, "previous run failed: {trimmed}...")
                } else {
                    write!(f, "previous run failed: {trimmed}")
                }
            }
        }
    }
}

/// Errors raised while computing a [`Plan`].
#[derive(Debug, Error)]
pub enum PlanError {
    /// The action graph contains a cycle. Carries the unsettled node ids.
    #[error("cycle detected: {0:?}")]
    Cycle(Vec<ActionId>),
    /// An action declares a dependency on an id not present in the action set.
    #[error("action {from} depends on {to}, which is not registered")]
    MissingDep {
        /// The action that declared the missing dep.
        from: ActionId,
        /// The id that was not found.
        to: ActionId,
    },
    /// Underlying [`Action::input_hash`] failure.
    #[error(transparent)]
    Build(#[from] BuildError),
}

impl Plan {
    /// Topological layers, each layer's actions independent of each other.
    #[must_use]
    pub fn layers(&self) -> &[Vec<ActionId>] {
        &self.layers
    }

    /// Map of stale action id → reason. Actions absent from the map are up-to-date.
    #[must_use]
    pub fn reasons(&self) -> &HashMap<ActionId, ChangeReason> {
        &self.reasons
    }

    /// Iterator over the ids of all stale actions (any reason).
    pub fn stale(&self) -> impl Iterator<Item = &ActionId> {
        self.reasons.keys()
    }

    /// Iterator over all action ids in topological order, flattening layers.
    pub fn flat_topo(&self) -> impl Iterator<Item = &ActionId> {
        self.layers.iter().flatten()
    }

    /// Build a plan from a set of actions and a cache snapshot.
    ///
    /// # Errors
    /// - [`PlanError::MissingDep`] when an action declares a dep on an
    ///   unknown id.
    /// - [`PlanError::Cycle`] when no topological order exists.
    /// - [`PlanError::Build`] when [`Action::input_hash`] returns an error.
    pub async fn compute(
        actions: &[Box<dyn Action>],
        cache: &dyn Cache,
        ctx: &Ctx,
    ) -> std::result::Result<Self, PlanError> {
        Self::compute_with_forced(actions, cache, ctx, &HashSet::new()).await
    }

    /// Same as [`Self::compute`], but every id in `forced` is stale
    /// regardless of what the cache says — `repolith sync --force`.
    ///
    /// Nothing else changes. In particular the [`ChangeReason::UpstreamMoved`]
    /// cascade is untouched, so forcing an action also re-runs whatever is
    /// built from it without any special handling here.
    ///
    /// # Errors
    /// Same as [`Self::compute`].
    pub async fn compute_with_forced(
        actions: &[Box<dyn Action>],
        cache: &dyn Cache,
        ctx: &Ctx,
        forced: &HashSet<ActionId>,
    ) -> std::result::Result<Self, PlanError> {
        // 1. Build inbound + dependents maps.
        let ids: HashSet<ActionId> = actions.iter().map(|a| a.id()).collect();
        let mut inbound: HashMap<ActionId, usize> = ids.iter().map(|i| (i.clone(), 0)).collect();
        let mut dependents: HashMap<ActionId, Vec<ActionId>> = HashMap::new();

        for a in actions {
            for dep in a.deps() {
                if !ids.contains(&dep) {
                    return Err(PlanError::MissingDep {
                        from: a.id(),
                        to: dep,
                    });
                }
                if let Some(c) = inbound.get_mut(&a.id()) {
                    *c += 1;
                }
                dependents.entry(dep).or_default().push(a.id());
            }
        }

        // 2. Layer-by-layer extraction (Kahn).
        let mut layers: Vec<Vec<ActionId>> = Vec::new();
        loop {
            let mut layer: Vec<ActionId> = inbound
                .iter()
                .filter(|(_, c)| **c == 0)
                .map(|(id, _)| id.clone())
                .collect();
            if layer.is_empty() {
                break;
            }
            layer.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic ordering within a layer
            for id in &layer {
                inbound.remove(id);
                if let Some(children) = dependents.get(id) {
                    for child in children.clone() {
                        if let Some(c) = inbound.get_mut(&child) {
                            *c -= 1;
                        }
                    }
                }
            }
            layers.push(layer);
        }
        if !inbound.is_empty() {
            let mut remaining: Vec<ActionId> = inbound.into_keys().collect();
            remaining.sort_by(|a, b| a.0.cmp(&b.0));
            return Err(PlanError::Cycle(remaining));
        }

        // 3. Compute ChangeReason per action, walking layers in order so a
        //    stale ancestor is already marked when we look at its children.
        //
        //    Within each layer the per-action `input_hash` and
        //    `cache.last_build` calls are independent — they may shell out
        //    (`git ls-remote`, `cargo --version`) so running them
        //    concurrently turns wall-clock from `sum(latency)` into
        //    `max(latency)`. The cascade pass that uses the results is
        //    sequential because layer N reads `stale` populated by layer
        //    N-1.
        let by_id: HashMap<ActionId, &dyn Action> =
            actions.iter().map(|a| (a.id(), a.as_ref())).collect();
        let mut reasons: HashMap<ActionId, ChangeReason> = HashMap::new();
        let mut stale: HashSet<ActionId> = HashSet::new();
        let mut input_hashes: HashMap<ActionId, Sha256> = HashMap::new();

        for layer in &layers {
            // Honor cancel between layers — the parallel probes inside
            // a layer (network, subprocess) can take seconds, so polling
            // here means a Ctrl-C during plan computation aborts before
            // the next layer fires its `git ls-remote` storm.
            if ctx.cancel.is_cancelled() {
                return Err(PlanError::Build(BuildError::Cancelled));
            }
            let LayerProbes {
                hashes_now,
                cached_map,
                presence_map,
            } = probe_layer(layer, &by_id, cache, ctx).await?;

            // Sequential cascade — order matters because `UpstreamMoved`
            // depends on the `stale` set populated by earlier ids.
            for id in layer {
                let now = hashes_now[id];
                input_hashes.insert(id.clone(), now);
                let last = cached_map.get(id).and_then(Option::as_ref);
                let present = presence_map.get(id).copied().unwrap_or(true);
                if let Some(reason) = classify(
                    id,
                    by_id[id],
                    now,
                    last,
                    &stale,
                    present,
                    forced.contains(id),
                ) {
                    reasons.insert(id.clone(), reason);
                    stale.insert(id.clone());
                }
            }
        }

        Ok(Self {
            layers,
            reasons,
            input_hashes,
        })
    }

    /// Pre-computed input hash for `id`, captured during [`Self::compute`].
    /// Returns `None` for ids not in the plan (caller error).
    #[must_use]
    pub fn input_hash(&self, id: &ActionId) -> Option<Sha256> {
        self.input_hashes.get(id).copied()
    }
}

/// Everything one layer's concurrent probes produced, keyed by action id.
struct LayerProbes {
    hashes_now: HashMap<ActionId, Sha256>,
    cached_map: HashMap<ActionId, Option<BuildEvent>>,
    presence_map: HashMap<ActionId, bool>,
}

/// Run all three per-action probes for one layer concurrently.
///
/// The probes are independent and may shell out (`git ls-remote`,
/// `cargo --version`, `docker image inspect`), so fanning them out turns
/// wall-clock from `sum(latency)` into `max(latency)`. Extracted from
/// [`Plan::compute`] to keep that function under the workspace's
/// `clippy::too_many_lines` threshold.
async fn probe_layer(
    layer: &[ActionId],
    by_id: &HashMap<ActionId, &dyn Action>,
    cache: &dyn Cache,
    ctx: &Ctx,
) -> std::result::Result<LayerProbes, PlanError> {
    // Pre-materialize (id, action) tuples so each future owns its own
    // pieces — capturing `by_id` in the closure hits the FnMut "moved
    // value" borrow checker because `.map()` reuses it across iterations.
    let hash_probes: Vec<(ActionId, &dyn Action)> =
        layer.iter().map(|id| (id.clone(), by_id[id])).collect();
    // Inner cancel: race each probe against `ctx.cancel`. The layer-edge
    // check in `compute` only catches cancel *between* layers; this
    // catches it *during* a fan-out (50 nodes mid-`git ls-remote`, Ctrl-C).
    let hash_results: Vec<(ActionId, Result<Sha256, BuildError>)> =
        join_all(hash_probes.into_iter().map(|(id, action)| async move {
            let probe = pin!(action.input_hash(ctx));
            let cancel = pin!(ctx.cancel.cancelled());
            let result = match select(probe, cancel).await {
                Either::Left((r, _)) => r,
                Either::Right(((), _)) => Err(BuildError::Cancelled),
            };
            (id, result)
        }))
        .await;

    // Is the artifact still on this machine? Cheap by contract (a stat, at
    // most one subprocess), so it rides along in the same fan-out for free.
    let presence_probes: Vec<(ActionId, &dyn Action)> =
        layer.iter().map(|id| (id.clone(), by_id[id])).collect();
    let presence: Vec<(ActionId, bool)> = join_all(
        presence_probes
            .into_iter()
            .map(|(id, action)| async move { (id, action.output_present(ctx).await) }),
    )
    .await;

    let cached: Vec<(ActionId, Option<BuildEvent>)> = join_all(layer.iter().map(|id| {
        let id = id.clone();
        async move { (id.clone(), cache.last_build(&id).await) }
    }))
    .await;

    let mut hashes_now: HashMap<ActionId, Sha256> = HashMap::new();
    for (id, result) in hash_results {
        hashes_now.insert(id, result?);
    }
    Ok(LayerProbes {
        hashes_now,
        cached_map: cached.into_iter().collect(),
        presence_map: presence.into_iter().collect(),
    })
}

/// Decide whether `id` is stale, given its freshly-computed input hash,
/// its last cached build event, and the set of already-stale ids in
/// earlier positions of the same layer (or earlier layers).
///
/// Returns `Some(reason)` when stale, `None` when up-to-date. Pure — no
/// side effects, no async — extracted from `Plan::compute` so the latter
/// stays under the workspace's `clippy::too_many_lines` threshold.
fn classify(
    id: &ActionId,
    action: &dyn Action,
    now: Sha256,
    last: Option<&BuildEvent>,
    stale: &HashSet<ActionId>,
    output_present: bool,
    forced: bool,
) -> Option<ChangeReason> {
    let _ = id; // referenced only for trace context in future revisions
    // An explicit request outranks every cache-derived reason. Checked
    // first so a forced action that also happens to have changed inputs
    // still reports `Forced` — the user asked, and that is the honest
    // answer to "why is this running".
    if forced {
        return Some(ChangeReason::Forced);
    }
    // No cache entry at all means `NoCachedBuild`; a recorded failure gets
    // its own reason below. Either way the action re-runs.
    let Some(event) = last else {
        return Some(ChangeReason::NoCachedBuild);
    };
    match event {
        // Still stale, still re-runs — only the label differs from
        // `NoCachedBuild`, so `status` can distinguish "never ran" from
        // "ran and blew up", which the cache has always known.
        BuildEvent::Failed { error, .. } => Some(ChangeReason::PreviousFailure {
            error: error.to_string(),
        }),
        BuildEvent::Success { input, .. } if *input != now => {
            Some(ChangeReason::InputHashChanged {
                from: *input,
                to: now,
            })
        }
        // Inputs match, so the cache *claims* this is done. Only trust it
        // if the artifact is actually here — the entry may have been
        // written by another machine through a shared cache backend.
        BuildEvent::Success { .. } if !output_present => Some(ChangeReason::OutputMissing),
        BuildEvent::Success { .. } => action
            .deps()
            .into_iter()
            .find(|d| stale.contains(d))
            .map(|dep| ChangeReason::UpstreamMoved { dep }),
    }
}
