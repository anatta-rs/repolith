//! Orchestrator — drives a [`Plan`] to completion.
//!
//! `Orchestrator` owns the actions, the cache, and the execution context;
//! `execute_plan` walks the plan layer by layer, fanning the stale actions
//! out into a [`FuturesUnordered`] capped by a [`tokio::sync::Semaphore`].
//! On the first error in [`ExecMode::FailFast`] mode, a shared
//! [`CancellationToken`] is fired so in-flight peers can short-circuit.
//! [`ExecMode::KeepGoing`] lets the current layer settle but still halts
//! before the next layer is started.
//!
//! **Implementation invariant**: this module never uses
//! [`futures::future::join_all`] — it would defeat `FailFast` because
//! `join_all` ignores cancellation and waits for every future to finish.

use futures::stream::{FuturesUnordered, StreamExt};
use repolith_core::action::Action;
use repolith_core::cache::{Cache, CacheError};
use repolith_core::manifest::Manifest;
use repolith_core::plan::{Plan, PlanError};
use repolith_core::types::{ActionId, BuildEvent, Ctx, ExecMode, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Top-level orchestrator. Construct via [`Orchestrator::builder`].
pub struct Orchestrator {
    actions: Vec<Box<dyn Action>>,
    cache: Box<dyn Cache>,
    /// Optional manifest snapshot — kept for downstream consumers (CLI, telemetry).
    pub manifest: Option<Manifest>,
    max_parallelism: usize,
    base_ctx: Ctx,
}

/// Errors raised while executing a [`Plan`].
#[derive(Debug, Error)]
pub enum ExecError {
    /// At least one action in a layer reported an error. The vector contains
    /// every event recorded **so far** (across all completed layers + the
    /// current failing layer), so callers can render a full report.
    #[error("layer failed: {} event(s) recorded", events.len())]
    LayerFailed {
        /// Every event recorded up to and including the failing layer.
        events: Vec<BuildEvent>,
    },
    /// Underlying [`Plan::compute`] failure surfaced through the orchestrator.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// Cache write failure while persisting an event.
    #[error(transparent)]
    Cache(#[from] CacheError),
}

/// Errors raised while validating a [`Builder`] configuration.
#[derive(Debug, Error)]
pub enum BuilderError {
    /// `Builder::cache(...)` was never called.
    #[error("orchestrator builder requires a cache")]
    MissingCache,
}

/// Builder for [`Orchestrator`]. Use [`Orchestrator::builder`] to obtain one.
pub struct Builder {
    actions: Vec<Box<dyn Action>>,
    cache: Option<Box<dyn Cache>>,
    manifest: Option<Manifest>,
    max_parallelism: usize,
    base_ctx: Ctx,
}

impl Orchestrator {
    /// Start a fresh [`Builder`].
    ///
    /// The default `max_parallelism` is `num_cpus::get()`, the default
    /// `base_ctx` uses a fresh [`CancellationToken`], `current_dir()` for
    /// `workdir`, and the inherited process env.
    #[must_use]
    pub fn builder() -> Builder {
        Builder {
            actions: vec![],
            cache: None,
            manifest: None,
            max_parallelism: num_cpus::get().max(1),
            base_ctx: Ctx {
                cancel: CancellationToken::new(),
                workdir: std::env::current_dir().unwrap_or_default(),
                env: std::env::vars().collect(),
            },
        }
    }

    /// Compute the [`Plan`] for the registered actions against the current
    /// cache state. Pure: no side effects.
    ///
    /// # Errors
    /// Forwarded from [`Plan::compute`].
    pub async fn compute_plan(&self) -> Result<Plan, PlanError> {
        Plan::compute(&self.actions, self.cache.as_ref(), &self.base_ctx).await
    }

    /// Execute every stale action in the plan, layer by layer.
    ///
    /// Returns the chronological list of [`BuildEvent`]s recorded across all
    /// executed layers. Cache writes happen **after** each layer settles, so
    /// a crash mid-layer leaves the prior layers persisted.
    ///
    /// # Errors
    /// - [`ExecError::LayerFailed`] when any action in a layer errors out
    ///   (further layers are not started, regardless of `mode`).
    /// - [`ExecError::Cache`] when persisting an event to the cache fails.
    pub async fn execute_plan(
        &mut self,
        plan: &Plan,
        mode: ExecMode,
    ) -> Result<Vec<BuildEvent>, ExecError> {
        let sem = Arc::new(Semaphore::new(self.max_parallelism));
        let mut all_events: Vec<BuildEvent> = Vec::new();

        for layer in plan.layers() {
            let stale: Vec<ActionId> = layer
                .iter()
                .filter(|id| plan.reasons().contains_key(*id))
                .cloned()
                .collect();
            if stale.is_empty() {
                continue;
            }

            let layer_events = self.execute_layer(&stale, mode, &sem).await;

            // Persist whatever this layer produced before deciding to halt.
            for ev in &layer_events {
                self.cache.record(ev.clone()).await?;
            }
            all_events.extend(layer_events.iter().cloned());

            if layer_events
                .iter()
                .any(|e| matches!(e, BuildEvent::Failed { .. }))
            {
                return Err(ExecError::LayerFailed { events: all_events });
            }
        }

        Ok(all_events)
    }

    /// Run one layer's actions concurrently, capped by `sem`. Never panics
    /// on errors — they're folded into [`BuildEvent::Failed`] entries.
    async fn execute_layer(
        &self,
        stale_ids: &[ActionId],
        mode: ExecMode,
        sem: &Arc<Semaphore>,
    ) -> Vec<BuildEvent> {
        // Fresh child token per layer — failing layer N doesn't poison layer N+1.
        let cancel = self.base_ctx.cancel.child_token();
        let layer_ctx = Ctx {
            cancel: cancel.clone(),
            workdir: self.base_ctx.workdir.clone(),
            env: self.base_ctx.env.clone(),
        };

        let by_id: HashMap<ActionId, &Box<dyn Action>> =
            self.actions.iter().map(|a| (a.id(), a)).collect();

        let mut pending: FuturesUnordered<_> = stale_ids
            .iter()
            .filter_map(|id| by_id.get(id).map(|a| (id.clone(), *a)))
            .map(|(id, action)| {
                let sem = Arc::clone(sem);
                let layer_ctx = layer_ctx.clone();
                async move {
                    // Acquire concurrency permit — yields when the layer is wider than the limit.
                    let _permit = sem.acquire().await.expect("semaphore never closed");
                    let started = Instant::now();
                    // Compute the input hash up front so we record it on both Success and Failed.
                    let input_hash =
                        action.input_hash(&layer_ctx).await.unwrap_or(Sha256([0; 32]));
                    let result = action.execute(&layer_ctx).await;
                    (id, started.elapsed(), input_hash, result)
                }
            })
            .collect();

        let mut events: Vec<BuildEvent> = Vec::new();

        while let Some((id, dur, input_hash, result)) = pending.next().await {
            let ms = u64::try_from(dur.as_millis()).unwrap_or(u64::MAX);
            match result {
                Ok(out) => events.push(BuildEvent::Success {
                    id,
                    input: input_hash,
                    output: out.output_hash,
                    ms,
                }),
                Err(e) => {
                    events.push(BuildEvent::Failed {
                        id,
                        input: input_hash,
                        error: e,
                        ms,
                    });
                    if mode == ExecMode::FailFast {
                        // Signal in-flight peers to short-circuit on their next .await poll.
                        cancel.cancel();
                    }
                }
            }
        }

        events
    }
}

impl Builder {
    /// Set the cache backend. **Required** — `build()` errors otherwise.
    #[must_use]
    pub fn cache<C: Cache + 'static>(mut self, c: C) -> Self {
        self.cache = Some(Box::new(c));
        self
    }

    /// Attach the parsed manifest snapshot (optional, for downstream consumers).
    #[must_use]
    pub fn manifest(mut self, m: Manifest) -> Self {
        self.manifest = Some(m);
        self
    }

    /// Cap on concurrent in-flight actions. Floored to `1` (zero is meaningless).
    #[must_use]
    pub fn max_parallelism(mut self, n: usize) -> Self {
        self.max_parallelism = n.max(1);
        self
    }

    /// Override the base [`Ctx`] (cancel token, workdir, env).
    #[must_use]
    pub fn base_ctx(mut self, ctx: Ctx) -> Self {
        self.base_ctx = ctx;
        self
    }

    /// Register one action. May be called many times.
    #[must_use]
    pub fn register<A: Action + 'static>(mut self, a: A) -> Self {
        self.actions.push(Box::new(a));
        self
    }

    /// Finalize.
    ///
    /// # Errors
    /// [`BuilderError::MissingCache`] when no cache was set.
    pub fn build(self) -> Result<Orchestrator, BuilderError> {
        let cache = self.cache.ok_or(BuilderError::MissingCache)?;
        Ok(Orchestrator {
            actions: self.actions,
            cache,
            manifest: self.manifest,
            max_parallelism: self.max_parallelism,
            base_ctx: self.base_ctx,
        })
    }
}

