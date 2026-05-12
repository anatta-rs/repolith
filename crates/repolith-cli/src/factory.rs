//! Translate a parsed [`Manifest`] into the concrete `Box<dyn Action>` set
//! the orchestrator consumes.
//!
//! Each `[[node]]` produces one [`Action`] per entry in its `actions` array,
//! identified as `{node.id}::{kind}::{idx}` (matches
//! [`Manifest::action_ids`](repolith_core::manifest::Manifest::action_ids)).
//! Actions within a single node are wired sequentially: `actions[i]` depends
//! on `actions[i-1]`. Cross-node dependencies are not expressible in the M1
//! manifest schema and therefore not generated here.

use anyhow::{Context, Result, bail};
use repolith_actions::cargo_install::{CargoInstall, CargoSource};
use repolith_actions::git_clone::GitClone;
use repolith_core::action::Action;
use repolith_core::manifest::{ActionEntry, Manifest};
use repolith_core::types::ActionId;
use std::path::PathBuf;

/// Default install root when an action does not specify one. Matches the
/// convention used by `~/.repolith/bin` in the manifest docs.
fn default_install_to() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".repolith")
}

/// Build the action list from a manifest.
///
/// # Errors
/// - A `kind = "git-clone"` action on a node without `git` URL.
/// - A `kind = "cargo-install"` action on a node with neither `git` nor `path`.
pub fn build_actions_from_manifest(manifest: &Manifest) -> Result<Vec<Box<dyn Action>>> {
    let mut out: Vec<Box<dyn Action>> = Vec::new();

    for node in &manifest.nodes {
        let mut prev_id: Option<ActionId> = None;

        for (idx, entry) in node.actions.iter().enumerate() {
            let kind = match entry {
                ActionEntry::GitClone => "git-clone",
                ActionEntry::CargoInstall { .. } => "cargo-install",
            };
            let id = ActionId(format!("{}::{}::{}", node.id, kind, idx));
            let deps = prev_id.iter().cloned().collect::<Vec<_>>();

            let action: Box<dyn Action> = match entry {
                ActionEntry::GitClone => {
                    let url = node.git.clone().with_context(|| {
                        format!(
                            "node `{}`: git-clone action requires `git` URL on the node",
                            node.id
                        )
                    })?;
                    let path = node
                        .path
                        .clone()
                        .unwrap_or_else(|| PathBuf::from(format!("./{}", node.id)));
                    Box::new(GitClone {
                        id: id.clone(),
                        repo_url: url,
                        path,
                        deps,
                    })
                }
                ActionEntry::CargoInstall {
                    crate_name,
                    features,
                    install_to,
                } => {
                    let source = if let Some(p) = &node.path {
                        CargoSource::Path { path: p.clone() }
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
                        id: id.clone(),
                        source,
                        crate_name: crate_name.clone().or_else(|| Some(node.id.clone())),
                        features: features.clone(),
                        install_to: install_to.clone().unwrap_or_else(default_install_to),
                        deps,
                    })
                }
            };

            out.push(action);
            prev_id = Some(id);
        }
    }

    Ok(out)
}
