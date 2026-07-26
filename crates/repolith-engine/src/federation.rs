//! Federation — `kind = "repolith"` runs a child stack's manifest as a
//! nested plan (orchestrator-of-orchestrators).
//!
//! `RepolithSync` is an `Action` that, at execute time, parses the
//! child manifest found inside the node's checkout, builds a nested
//! [`Orchestrator`](crate::orchestrator::Orchestrator) over it, and runs
//! it to completion. The child stack keeps its **own local cache** —
//! exactly what a human would get running `repolith sync` in that
//! directory — so parent cache ids never collide with child ids.
//!
//! # Guard rails
//!
//! - **Cycle detection**: every `RepolithSync` carries the canonicalized
//!   manifest paths of its ancestors. A child manifest already in that
//!   chain aborts with the full offending chain in the error.
//! - **Depth bound**: `MAX_FEDERATION_DEPTH` levels, then a hard error.
//! - **Cancellation tree**: the child orchestrator's root token is a
//!   `child_token()` of the parent's `Ctx` token — Ctrl-C at any level
//!   reaps the whole tree, subprocess groups included.
//! - **Global concurrency**: the child draws permits from the *parent's*
//!   semaphore (see `Builder::shared_semaphore`), so `--jobs N` bounds the
//!   whole tree. `RepolithSync` itself is a coordinator
//!   (`Action::is_coordinator`) and is never charged a permit.
//!
//! # Wiring (dependency direction)
//!
//! Turning a parsed `Manifest` into concrete actions requires the
//! action implementations (`repolith-actions`) and a cache backend
//! (`repolith-cache`) — both out of reach from this crate without
//! inverting the dependency graph. The CLI injects them through
//! `FederationHooks`; recursion happens naturally because the CLI's
//! factory constructs nested `RepolithSync` values with the same hooks.

use crate::orchestrator::Orchestrator;
use async_trait::async_trait;
use repolith_core::action::Action;
use repolith_core::cache::Cache;
use repolith_core::manifest::Manifest;
use repolith_core::paths::contain_within;
use repolith_core::types::{ActionId, BuildError, BuildEvent, BuildOutput, Ctx, ExecMode, Sha256};
use sha2::{Digest, Sha256 as ShaHasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Semaphore;

/// Maximum number of nested federation levels. Deep enough for any sane
/// stack-of-stacks; shallow enough that a runaway recursion fails fast
/// with a readable chain instead of exhausting the filesystem.
pub const MAX_FEDERATION_DEPTH: usize = 8;

/// Environment injected by the host (CLI) — everything `RepolithSync`
/// cannot construct without inverting the crate dependency graph.
pub trait FederationHooks: Send + Sync {
    /// Turn a parsed child manifest into concrete actions.
    ///
    /// `chain` already includes the child manifest's canonical path (last
    /// element); implementations pass it through to any nested
    /// `RepolithSync` they construct so cycle detection stays airtight.
    ///
    /// # Errors
    /// Implementation-defined (missing node source, unknown kind wiring…).
    fn build_actions(
        &self,
        manifest: &Manifest,
        stack_dir: &Path,
        chain: &[PathBuf],
    ) -> Result<Vec<Box<dyn Action>>, BuildError>;

    /// Open (or create) the child stack's **local** cache.
    ///
    /// # Errors
    /// Backend-defined (I/O, schema migration…).
    fn open_cache(&self, stack_dir: &Path) -> Result<Box<dyn Cache>, BuildError>;
}

/// Action that syncs a child repolith stack found in the node's checkout.
pub struct RepolithSync {
    /// Action identifier for the plan.
    pub id: ActionId,
    /// The node's checkout directory — containment boundary for `manifest_rel`.
    pub node_path: PathBuf,
    /// Child manifest path relative to [`Self::node_path`]. `None` = `repolith.toml`.
    pub manifest_rel: Option<PathBuf>,
    /// Canonicalized manifest paths of every ancestor stack (cycle guard).
    pub chain: Vec<PathBuf>,
    /// Execution mode forwarded to the child orchestrator.
    pub mode: ExecMode,
    /// The federation-wide concurrency pool (parent's semaphore).
    pub shared_sem: Arc<Semaphore>,
    /// Host-injected wiring (action factory + cache opener).
    pub hooks: Arc<dyn FederationHooks>,
    /// IDs of actions that must complete before this one runs.
    pub deps: Vec<ActionId>,
}

impl RepolithSync {
    /// Canonicalized child manifest path, proven inside the checkout, with
    /// cycle + depth guards applied.
    fn resolved_manifest(&self) -> Result<PathBuf, BuildError> {
        let rel = self
            .manifest_rel
            .as_deref()
            .unwrap_or(Path::new("repolith.toml"));
        let manifest = contain_within(&self.node_path, rel)?;
        if let Some(pos) = self.chain.iter().position(|p| p == &manifest) {
            let mut cycle: Vec<String> = self.chain[pos..]
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            cycle.push(manifest.display().to_string());
            return Err(BuildError::Io(format!(
                "federation cycle: {}",
                cycle.join(" -> ")
            )));
        }
        if self.chain.len() >= MAX_FEDERATION_DEPTH {
            return Err(BuildError::Io(format!(
                "federation depth {} exceeds the maximum of {MAX_FEDERATION_DEPTH} (chain: {})",
                self.chain.len() + 1,
                self.chain
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" -> ")
            )));
        }
        Ok(manifest)
    }
}

#[async_trait]
impl Action for RepolithSync {
    fn id(&self) -> ActionId {
        self.id.clone()
    }

    fn deps(&self) -> Vec<ActionId> {
        self.deps.clone()
    }

    fn is_coordinator(&self) -> bool {
        true
    }

    async fn input_hash(&self, ctx: &Ctx) -> Result<Sha256, BuildError> {
        let mut h = ShaHasher::new();
        h.update(b"repolith:manifest:");

        // Pre-clone grace: when the checkout (or its manifest) does not
        // exist yet — a sibling git-clone earlier in the same node will
        // materialize it — hash a deterministic marker instead of failing
        // the whole plan. The action is stale anyway (NoCachedBuild or
        // UpstreamMoved cascade), executes after the clone, and the next
        // sync re-hashes the real bytes (one extra, harmless re-run).
        match self.resolved_manifest() {
            Ok(manifest) => {
                let bytes = tokio::fs::read(&manifest).await.map_err(|e| {
                    BuildError::Io(format!("read manifest {}: {e}", manifest.display()))
                })?;
                h.update(&bytes);

                // Checkout state: HEAD when the stack is a git worktree.
                let mut head = Command::new("git");
                head.args(["-C"])
                    .arg(&self.node_path)
                    .args(["rev-parse", "HEAD"]);
                let out = tokio::select! {
                    r = head.output() => {
                        r.map_err(|e| BuildError::Io(format!("spawn git rev-parse: {e}")))?
                    }
                    () = ctx.cancel.cancelled() => return Err(BuildError::Cancelled),
                };
                h.update(b":checkout:");
                if out.status.success() {
                    h.update(&out.stdout);
                } else {
                    h.update(b"no-git");
                }
            }
            Err(_) if !self.node_path.exists() => {
                h.update(b"pre-clone");
            }
            Err(e) => return Err(e),
        }
        Ok(Sha256(h.finalize().into()))
    }

    async fn execute(&self, ctx: &Ctx) -> Result<BuildOutput, BuildError> {
        // Strict at execute time — the checkout must exist now.
        let manifest_path = self.resolved_manifest()?;
        let stack_dir = manifest_path
            .parent()
            .ok_or_else(|| {
                BuildError::Io(format!(
                    "manifest {} has no parent",
                    manifest_path.display()
                ))
            })?
            .to_path_buf();

        let src = tokio::fs::read_to_string(&manifest_path)
            .await
            .map_err(|e| {
                BuildError::Io(format!("read manifest {}: {e}", manifest_path.display()))
            })?;
        let manifest = Manifest::from_toml(&src).map_err(|e| {
            BuildError::Io(format!("child manifest {}: {e}", manifest_path.display()))
        })?;

        // Extended chain: ancestors + this child. Passed to the hooks so
        // nested RepolithSync instances inherit the full lineage.
        let mut chain = self.chain.clone();
        chain.push(manifest_path.clone());

        let actions = self.hooks.build_actions(&manifest, &stack_dir, &chain)?;
        let cache = self.hooks.open_cache(&stack_dir)?;

        let child_ctx = Ctx {
            // Child token: cancelling the parent cancels every level below.
            cancel: ctx.cancel.child_token(),
            workdir: stack_dir.clone(),
            env: ctx.env.clone(),
        };

        let mut builder = Orchestrator::builder()
            .manifest(manifest)
            .base_ctx(child_ctx)
            .shared_semaphore(Arc::clone(&self.shared_sem));
        for a in actions {
            builder = builder.register_boxed(a);
        }
        let mut orch = builder
            .cache_boxed(cache)
            .build()
            .map_err(|e| BuildError::Io(format!("child orchestrator: {e}")))?;

        let plan = orch.compute_plan().await.map_err(|e| {
            BuildError::Io(format!("child plan ({}): {e}", manifest_path.display()))
        })?;
        let events = orch.execute_plan(&plan, self.mode).await.map_err(|e| {
            BuildError::Io(format!("child sync ({}): {e}", manifest_path.display()))
        })?;

        // output_hash: digest of the child's (id, output) pairs in stable
        // order — identical child results yield an identical parent hash.
        let mut pairs: Vec<(String, [u8; 32])> = events
            .iter()
            .filter_map(|e| match e {
                BuildEvent::Success { id, output, .. } => Some((id.0.clone(), output.0)),
                BuildEvent::Failed { .. } => None,
            })
            .collect();
        pairs.sort();
        let mut h = ShaHasher::new();
        h.update(b"repolith:events:");
        for (id, out) in &pairs {
            h.update(id.as_bytes());
            h.update(b"=");
            h.update(out);
        }
        Ok(BuildOutput {
            output_hash: Sha256(h.finalize().into()),
            stdout: format!(
                "child stack {} synced — {} action(s)",
                stack_dir.display(),
                pairs.len()
            ),
        })
    }
}
