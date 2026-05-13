//! Repolith manifest schema and parser.
//!
//! The manifest (typically `repolith.toml` at the repo root) is the user-facing
//! contract: it declares which upstream nodes to track and which actions to run
//! against each.
//!
//! Parse via [`Manifest::from_toml`], which both deserializes and validates.
//!
//! [`Manifest::from_toml`]: crate::manifest::Manifest::from_toml

use crate::types::ActionId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

/// Top-level `[orchestrator]` section.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OrchestratorMeta {
    /// Manifest schema version, expected to match `~0.1` (semver tilde range).
    pub schema_version: String,
    /// Human-readable name for the orchestrator instance (e.g. `"anatta-rs"`).
    pub name: String,
}

/// A single `[[node]]` entry — one upstream repository to manage.
///
/// **`git` and `path` semantics depend on the action kind**:
///
/// - For `kind = "git-clone"` actions, `git` is the **source** URL
///   (required) and `path` is the **destination** working tree
///   (optional, defaults to `./{node.id}`). Both fields can — and
///   typically do — coexist on the same node.
/// - For `kind = "cargo-install"` actions, `path` (when set) is the
///   **source** crate root that wins over `git`. If only `git` is
///   set, cargo is invoked with `--git <url>`. If both are set, `path`
///   takes precedence — the assumption being you've already cloned via
///   a sibling `git-clone` action.
///
/// The validator only requires that **at least one** of `git`/`path`
/// is set — the per-action interpretation above lives in the CLI's
/// manifest factory, not here.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NodeEntry {
    /// Unique identifier for the node within the manifest.
    pub id: String,
    /// Remote git URL. Required when [`Self::path`] is `None` and the
    /// node has a `git-clone` action; required as the cargo-install
    /// fallback when `path` is absent. URLs must use one of the
    /// allowlisted schemes (`https://`, `http://`, `ssh://`, `git@`,
    /// `file://`) and must not start with `-` — see the validator.
    pub git: Option<String>,
    /// Local path. For `git-clone` it's the destination working tree;
    /// for `cargo-install` it's the source crate root (preferred over
    /// `git`). Tilde expansion (`~/...`) is applied by the CLI before
    /// the path reaches `git` / `cargo`.
    pub path: Option<PathBuf>,
    /// Ordered list of actions to run for this node.
    #[serde(default, rename = "action")]
    pub actions: Vec<ActionEntry>,
}

/// Root manifest document — deserialized from `repolith.toml`.
///
/// `[[attached]]` blocks are accepted in the TOML for forward
/// compatibility but currently ignored (no field is collected). The
/// `attached` schema is reserved for an M2 `template_apply` action and
/// will land with that feature.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    /// `[orchestrator]` metadata (schema version, instance name).
    pub orchestrator: OrchestratorMeta,
    /// All `[[node]]` entries declared in the manifest.
    #[serde(default, rename = "node")]
    pub nodes: Vec<NodeEntry>,
}

/// Tagged variant of `[[node.action]]`, dispatched by the TOML `kind` field
/// (kebab-case: `"git-clone"`, `"cargo-install"`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ActionEntry {
    /// `kind = "git-clone"` — fetch the node's source.
    GitClone,
    /// `kind = "cargo-install"` — `cargo install` from the node's source tree.
    CargoInstall {
        /// Crate name (TOML key `crate`). Defaults to the node's `id` when absent.
        #[serde(default, rename = "crate")]
        crate_name: Option<String>,
        /// Cargo features to enable.
        #[serde(default)]
        features: Vec<String>,
        /// Target install directory. Defaults to `~/.repolith/bin` when absent.
        #[serde(default)]
        install_to: Option<PathBuf>,
    },
}

/// Errors that can occur while parsing or validating a manifest.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// Underlying TOML deserialization failed (syntax error, type mismatch, unknown variant).
    #[error("toml parse: {0}")]
    Toml(#[from] toml::de::Error),
    /// `orchestrator.schema_version` does not satisfy the `~0.1` requirement.
    #[error("schema version {got}: requires ~0.1")]
    SchemaMismatch {
        /// The offending version string from the manifest.
        got: String,
    },
    /// Two or more `[[node]]` entries share the same `id`.
    #[error("duplicate node id: {0}")]
    DuplicateNodeId(String),
    /// A `[[node]]` declares neither a `git` URL nor a local `path`.
    #[error("node {0} has neither `git` nor `path`")]
    NodeMissingSource(String),
    /// A user-supplied URL would be interpreted as a CLI flag by the
    /// downstream `git` / `cargo` invocation (CWE-88, argument injection).
    #[error("invalid URL in node `{node}`: {reason}")]
    InvalidUrl {
        /// Node id that owns the offending URL.
        node: String,
        /// Why the URL was rejected (leading dash, unknown scheme, …).
        reason: String,
    },
    /// A user-supplied identifier (crate name, feature name) would be
    /// interpreted as a CLI flag or break shell argument parsing.
    #[error("invalid argument in node `{node}`: {reason}")]
    InvalidArg {
        /// Node id that owns the offending field.
        node: String,
        /// Why the value was rejected.
        reason: String,
    },
}

/// URL schemes the orchestrator considers safe to hand to `git` and `cargo`.
/// Anything else is rejected at parse time so a malicious manifest can't
/// reach the subprocess layer.
const ALLOWED_URL_SCHEMES: &[&str] = &["https://", "http://", "ssh://", "git@", "file://"];

/// Reject strings that `git` / `cargo` would interpret as a CLI flag
/// because they begin with `-`. The check is intentionally permissive
/// for everything else — the goal is defense against trivial argument
/// injection, not validation of the entire URL grammar.
fn check_no_leading_dash(value: &str, node: &str, field: &str) -> Result<(), ManifestError> {
    if value.starts_with('-') {
        return Err(ManifestError::InvalidArg {
            node: node.to_string(),
            reason: format!("`{field}` starts with `-`, would be parsed as a CLI flag"),
        });
    }
    Ok(())
}

/// Validate `url` against [`ALLOWED_URL_SCHEMES`] and the no-leading-dash
/// rule. Empty URLs are rejected to keep `git ls-remote ""` from
/// producing confusing diagnostics.
fn check_url(url: &str, node: &str) -> Result<(), ManifestError> {
    if url.is_empty() {
        return Err(ManifestError::InvalidUrl {
            node: node.to_string(),
            reason: "URL is empty".into(),
        });
    }
    if url.starts_with('-') {
        return Err(ManifestError::InvalidUrl {
            node: node.to_string(),
            reason: format!("`{url}` starts with `-`, would be parsed as a CLI flag"),
        });
    }
    if !ALLOWED_URL_SCHEMES
        .iter()
        .any(|scheme| url.starts_with(scheme))
    {
        return Err(ManifestError::InvalidUrl {
            node: node.to_string(),
            reason: format!("scheme not in allowlist {ALLOWED_URL_SCHEMES:?} (got `{url}`)"),
        });
    }
    Ok(())
}

impl Manifest {
    /// Parse and validate a manifest from a TOML source string.
    ///
    /// Returns a fully-validated [`Manifest`] on success. Validation covers
    /// schema version range, node-id uniqueness, and the per-node "must have
    /// `git` or `path`" rule.
    ///
    /// # Errors
    /// See [`ManifestError`] for the validation rules and the corresponding variants.
    pub fn from_toml(src: &str) -> Result<Self, ManifestError> {
        let m: Manifest = toml::from_str(src)?;
        m.validate()?;
        Ok(m)
    }

    /// Run all post-deserialize semantic checks. Called by [`Self::from_toml`].
    fn validate(&self) -> Result<(), ManifestError> {
        // schema_version is in 0.1.x range
        let req = semver::VersionReq::parse("~0.1").unwrap();
        let v = semver::Version::parse(&format!("{}.0", self.orchestrator.schema_version))
            .map_err(|_| ManifestError::SchemaMismatch {
                got: self.orchestrator.schema_version.clone(),
            })?;
        if !req.matches(&v) {
            return Err(ManifestError::SchemaMismatch {
                got: self.orchestrator.schema_version.clone(),
            });
        }
        // Per-node checks: id uniqueness, source presence, then the
        // argument-injection sweep on every user-controlled string field
        // that ultimately reaches a subprocess.
        let mut seen = std::collections::HashSet::new();
        for n in &self.nodes {
            if !seen.insert(&n.id) {
                return Err(ManifestError::DuplicateNodeId(n.id.clone()));
            }
            if n.git.is_none() && n.path.is_none() {
                return Err(ManifestError::NodeMissingSource(n.id.clone()));
            }
            if let Some(url) = &n.git {
                check_url(url, &n.id)?;
            }
            for action in &n.actions {
                if let ActionEntry::CargoInstall {
                    crate_name,
                    features,
                    ..
                } = action
                {
                    if let Some(name) = crate_name {
                        check_no_leading_dash(name, &n.id, "crate")?;
                        if name.contains(',') {
                            return Err(ManifestError::InvalidArg {
                                node: n.id.clone(),
                                reason: format!(
                                    "`crate` name `{name}` contains `,` which would split cargo's --features list"
                                ),
                            });
                        }
                    }
                    for feature in features {
                        check_no_leading_dash(feature, &n.id, "features entry")?;
                        if feature.contains(',') {
                            return Err(ManifestError::InvalidArg {
                                node: n.id.clone(),
                                reason: format!(
                                    "feature `{feature}` contains `,` which would split cargo's --features list"
                                ),
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Enumerate every action across every node as a stable [`ActionId`].
    ///
    /// Each id has the shape `"{node.id}::{action-kind}::{index}"`, where
    /// `index` is the action's position within its node (0-based). Ordering
    /// matches manifest declaration order; downstream consumers (Plan, Cache)
    /// rely on this for deterministic builds.
    #[must_use]
    pub fn action_ids(&self) -> Vec<ActionId> {
        self.nodes
            .iter()
            .flat_map(|n| {
                n.actions
                    .iter()
                    .enumerate()
                    .map(move |(i, a)| ActionId(format!("{}::{}::{}", n.id, action_kind(a), i)))
            })
            .collect()
    }
}

/// Kebab-case discriminator string for an [`ActionEntry`], matching
/// the TOML `kind` tag. Public so the CLI's manifest factory uses the
/// same single source of truth — adding a new action variant therefore
/// only needs touching this match plus the variant declaration.
#[must_use]
pub fn action_kind(a: &ActionEntry) -> &'static str {
    match a {
        ActionEntry::GitClone => "git-clone",
        ActionEntry::CargoInstall { .. } => "cargo-install",
    }
}
