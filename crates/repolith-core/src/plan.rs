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
use std::collections::{HashMap, HashSet};
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
        let by_id: HashMap<ActionId, &Box<dyn Action>> =
            actions.iter().map(|a| (a.id(), a)).collect();
        let mut reasons: HashMap<ActionId, ChangeReason> = HashMap::new();
        let mut stale: HashSet<ActionId> = HashSet::new();

        for layer in &layers {
            for id in layer {
                let a = by_id[id];
                let now = a.input_hash(ctx).await?;
                match cache.last_build(id).await {
                    None => {
                        reasons.insert(id.clone(), ChangeReason::NoCachedBuild);
                        stale.insert(id.clone());
                    }
                    Some(BuildEvent::Failed { .. }) => {
                        // Failed builds always re-run.
                        reasons.insert(id.clone(), ChangeReason::NoCachedBuild);
                        stale.insert(id.clone());
                    }
                    Some(BuildEvent::Success { input, .. }) if input != now => {
                        reasons.insert(
                            id.clone(),
                            ChangeReason::InputHashChanged {
                                from: input,
                                to: now,
                            },
                        );
                        stale.insert(id.clone());
                    }
                    Some(BuildEvent::Success { .. }) => {
                        // Hash unchanged — cascade if any dep is stale.
                        if let Some(stale_dep) = a.deps().into_iter().find(|d| stale.contains(d)) {
                            reasons
                                .insert(id.clone(), ChangeReason::UpstreamMoved { dep: stale_dep });
                            stale.insert(id.clone());
                        }
                    }
                }
            }
        }

        Ok(Self { layers, reasons })
    }
}
