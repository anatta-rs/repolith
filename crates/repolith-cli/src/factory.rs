//! Translate a parsed [`Manifest`] into the concrete `Box<dyn Action>` set
//! the orchestrator consumes.
//!
//! Each `[[node]]` produces one [`Action`] per entry in its `actions` array,
//! identified as `{node.id}::{kind}::{idx}` (matches
//! [`Manifest::action_ids`](repolith_core::manifest::Manifest::action_ids)).
//! Actions within a single node are wired sequentially: `actions[i]` depends
//! on `actions[i-1]`. Cross-node dependencies are not expressible in the M1
//! manifest schema and therefore not generated here.
//!
//! # Federation
//!
//! `kind = "repolith"` produces a [`RepolithSync`] coordinator. Its
//! [`FederationHooks`] implementation lives here ([`CliFederationHooks`]):
//! it calls this same factory recursively for child manifests (extending
//! the ancestor chain) and opens each child stack's **local** `SQLite` cache
//! at `<stack>/.repolith/cache.db`. Relative node paths resolve against
//! the manifest's own directory (`FactoryCtx::base_dir`), so a child
//! stack's `path = "../libs/x"` means "next to the child stack", not
//! "next to wherever the parent process was started".

use anyhow::{Context, Result, bail};
use repolith_actions::cargo_install::{CargoInstall, CargoSource};
use repolith_actions::docker::DockerBuild;
use repolith_actions::git_clone::GitClone;
use repolith_actions::paths::expand_tilde;
use repolith_cache::SqliteCache;
use repolith_core::action::Action;
use repolith_core::cache::Cache;
use repolith_core::manifest::{ActionEntry, Manifest, NodeEntry, action_kind};
use repolith_core::types::{ActionId, BuildError, ExecMode};
use repolith_engine::federation::{FederationHooks, RepolithSync};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use tokio::sync::Semaphore;

/// Default install root when an action does not specify one. Matches the
/// convention used by `~/.repolith/bin` in the manifest docs.
fn default_install_to() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".repolith")
}

/// Everything the factory needs beyond the manifest itself.
pub struct FactoryCtx {
    /// Directory the manifest lives in — relative node paths resolve
    /// against it.
    pub base_dir: PathBuf,
    /// Canonicalized manifest paths from the root stack down to (and
    /// including) the manifest being built. Cycle guard for federation.
    pub chain: Vec<PathBuf>,
    /// Execution mode forwarded to child orchestrators.
    pub mode: ExecMode,
    /// Federation-wide concurrency pool.
    pub sem: Arc<Semaphore>,
    /// Hook implementation handed to every `RepolithSync`.
    pub hooks: Arc<dyn FederationHooks>,
}

/// Resolve a node-relative path against the manifest's directory.
/// `expand_tilde` first so `~/x` stays absolute; `join` on an absolute
/// path replaces the base entirely.
fn resolve_node_path(base_dir: &Path, p: &Path) -> PathBuf {
    base_dir.join(expand_tilde(p))
}

/// Build the action list from a manifest.
///
/// # Errors
/// - A `kind = "git-clone"` action on a node without `git` URL.
/// - A `kind = "cargo-install"` action on a node with neither `git` nor `path`.
/// - A `kind = "docker"` / `kind = "repolith"` action on a node without `path`.
pub fn build_actions_from_manifest(
    manifest: &Manifest,
    fctx: &FactoryCtx,
) -> Result<Vec<Box<dyn Action>>> {
    let mut out: Vec<Box<dyn Action>> = Vec::new();

    for node in &manifest.nodes {
        let mut prev_id: Option<ActionId> = None;

        for (idx, entry) in node.actions.iter().enumerate() {
            // Reuse the manifest's action_kind() so adding a new variant
            // only requires touching one match arm (in manifest.rs).
            let kind = action_kind(entry);
            let id = ActionId(format!("{}::{}::{}", node.id, kind, idx));
            let deps = prev_id.iter().cloned().collect::<Vec<_>>();

            out.push(action_for_entry(node, entry, id.clone(), deps, fctx)?);
            prev_id = Some(id);
        }
    }

    Ok(out)
}

/// Node `path` required by `kind` — resolved against the manifest's dir,
/// with a kind-specific error when absent. The manifest validator already
/// guarantees presence; failing here too keeps manually-constructed
/// `Manifest` values from bypassing it.
fn required_node_path(node: &NodeEntry, fctx: &FactoryCtx, what: &str) -> Result<PathBuf> {
    node.path
        .as_deref()
        .map(|p| resolve_node_path(&fctx.base_dir, p))
        .with_context(|| format!("node `{}`: {what}", node.id))
}

/// One concrete action for one `[[node.action]]` entry.
fn action_for_entry(
    node: &NodeEntry,
    entry: &ActionEntry,
    id: ActionId,
    deps: Vec<ActionId>,
    fctx: &FactoryCtx,
) -> Result<Box<dyn Action>> {
    Ok(match entry {
        ActionEntry::GitClone => {
            let url = node.git.clone().with_context(|| {
                format!(
                    "node `{}`: git-clone action requires `git` URL on the node",
                    node.id
                )
            })?;
            let path = node.path.as_deref().map_or_else(
                || fctx.base_dir.join(&node.id),
                |p| resolve_node_path(&fctx.base_dir, p),
            );
            Box::new(GitClone {
                id,
                repo_url: url,
                path,
                deps,
            })
        }
        ActionEntry::CargoInstall {
            crate_name,
            package,
            profile,
            features,
            install_to,
        } => {
            let source = if let Some(p) = &node.path {
                CargoSource::Path {
                    path: resolve_node_path(&fctx.base_dir, p),
                }
            } else if let Some(url) = &node.git {
                CargoSource::Git {
                    url: url.clone(),
                    branch: None,
                }
            } else {
                bail!(
                    "node `{}`: cargo-install action requires either `git` or `path` on the node",
                    node.id
                );
            };
            Box::new(CargoInstall {
                id,
                source,
                crate_name: crate_name.clone().or_else(|| Some(node.id.clone())),
                package: package.clone(),
                profile: profile.clone(),
                features: features.clone(),
                install_to: install_to
                    .as_deref()
                    .map_or_else(default_install_to, expand_tilde),
                deps,
            })
        }
        ActionEntry::Docker {
            tag,
            dockerfile,
            context,
        } => {
            let node_path = required_node_path(
                node,
                fctx,
                "docker action requires `path` on the node (build context base)",
            )?;
            Box::new(DockerBuild {
                id,
                tag: tag.clone(),
                dockerfile: dockerfile.clone(),
                context: context.clone(),
                node_path,
                deps,
            })
        }
        ActionEntry::Repolith { manifest: rel } => {
            let node_path = required_node_path(
                node,
                fctx,
                "repolith action requires `path` on the node (child stack root)",
            )?;
            Box::new(RepolithSync {
                id,
                node_path,
                manifest_rel: rel.clone(),
                chain: fctx.chain.clone(),
                mode: fctx.mode,
                shared_sem: Arc::clone(&fctx.sem),
                hooks: Arc::clone(&fctx.hooks),
                deps,
            })
        }
    })
}

/// [`FederationHooks`] backed by this factory + local `SqliteCache`s.
///
/// Holds a `Weak` back-reference to itself (via [`Self::new`]'s
/// `Arc::new_cyclic`) so nested `RepolithSync` instances receive the same
/// hooks and recursion Just Works at any depth.
pub struct CliFederationHooks {
    mode: ExecMode,
    sem: Arc<Semaphore>,
    me: Weak<CliFederationHooks>,
}

impl CliFederationHooks {
    /// Build the hooks as a self-referencing `Arc`.
    pub fn new(mode: ExecMode, sem: Arc<Semaphore>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            mode,
            sem,
            me: me.clone(),
        })
    }
}

impl FederationHooks for CliFederationHooks {
    fn build_actions(
        &self,
        manifest: &Manifest,
        stack_dir: &Path,
        chain: &[PathBuf],
    ) -> std::result::Result<Vec<Box<dyn Action>>, BuildError> {
        let hooks = self
            .me
            .upgrade()
            .expect("hooks alive while an action holds them");
        let fctx = FactoryCtx {
            base_dir: stack_dir.to_path_buf(),
            chain: chain.to_vec(),
            mode: self.mode,
            sem: Arc::clone(&self.sem),
            hooks,
        };
        build_actions_from_manifest(manifest, &fctx)
            .map_err(|e| BuildError::Io(format!("child factory ({}): {e:#}", stack_dir.display())))
    }

    fn open_cache(&self, stack_dir: &Path) -> std::result::Result<Box<dyn Cache>, BuildError> {
        let dir = stack_dir.join(".repolith");
        std::fs::create_dir_all(&dir)
            .map_err(|e| BuildError::Io(format!("create {}: {e}", dir.display())))?;
        let path = dir.join("cache.db");
        let cache = SqliteCache::open(&path)
            .map_err(|e| BuildError::Io(format!("open child cache {}: {e}", path.display())))?;
        Ok(Box::new(cache))
    }
}
