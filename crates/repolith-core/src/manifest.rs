//! Repolith manifest schema and parser.
//!
//! The manifest (typically `repolith.toml` at the repo root) is the user-facing
//! contract: it declares which upstream nodes to track and which actions to run
//! against each.
//!
//! Parse via [`Manifest::from_toml`], which both deserializes and validates.

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
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NodeEntry {
    /// Unique identifier for the node within the manifest.
    pub id: String,
    /// Remote git URL. Required when [`Self::path`] is `None`.
    pub git: Option<String>,
    /// Local sibling clone path. Required when [`Self::git`] is `None`.
    pub path: Option<PathBuf>,
    /// Ordered list of actions to run for this node.
    #[serde(default, rename = "action")]
    pub actions: Vec<ActionEntry>,
}

/// A single `[[attached]]` entry — an external project linked into the orchestrator.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AttachedEntry {
    /// Filesystem path of the attached project.
    pub path: PathBuf,
    /// Data flow direction relative to the orchestrator.
    pub direction: Direction,
}

/// Root manifest document — deserialized from `repolith.toml`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    /// `[orchestrator]` metadata (schema version, instance name).
    pub orchestrator: OrchestratorMeta,
    /// All `[[node]]` entries declared in the manifest.
    #[serde(default, rename = "node")]
    pub nodes: Vec<NodeEntry>,
    /// All `[[attached]]` entries declared in the manifest.
    #[serde(default, rename = "attached")]
    pub attached: Vec<AttachedEntry>,
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
        #[serde(default, rename = "crate")] crate_name: Option<String>,
        /// Cargo features to enable.
        #[serde(default)] features: Vec<String>,
        /// Target install directory. Defaults to `~/.repolith/bin` when absent.
        #[serde(default)] install_to: Option<PathBuf>,
    },
}

/// Direction of data flow for an attached project. M1 only uses [`Direction::Inbound`].
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Data flows from the attached project into the orchestrator.
    Inbound,
    /// Data flows from the orchestrator out to the attached project. Reserved for post-M1.
    Outbound,
}

/// Errors that can occur while parsing or validating a manifest.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// Underlying TOML deserialization failed (syntax error, type mismatch, unknown variant).
    #[error("toml parse: {0}")] Toml(#[from] toml::de::Error),
    /// `orchestrator.schema_version` does not satisfy the `~0.1` requirement.
    #[error("schema version {got}: requires ~0.1")]
    SchemaMismatch {
        /// The offending version string from the manifest.
        got: String,
    },
    /// Two or more `[[node]]` entries share the same `id`.
    #[error("duplicate node id: {0}")] DuplicateNodeId(String),
    /// A `[[node]]` declares neither a `git` URL nor a local `path`.
    #[error("node {0} has neither `git` nor `path`")] NodeMissingSource(String),
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
            .map_err(|_| ManifestError::SchemaMismatch { got: self.orchestrator.schema_version.clone() })?;
        if !req.matches(&v) {
            return Err(ManifestError::SchemaMismatch { got: self.orchestrator.schema_version.clone() });
        }
        // unique node ids
        let mut seen = std::collections::HashSet::new();
        for n in &self.nodes {
            if !seen.insert(&n.id) {
                return Err(ManifestError::DuplicateNodeId(n.id.clone()));
            }
            if n.git.is_none() && n.path.is_none() {
                return Err(ManifestError::NodeMissingSource(n.id.clone()));
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
        self.nodes.iter()
            .flat_map(|n| n.actions.iter().enumerate()
                .map(move |(i, a)| ActionId(format!("{}::{}::{}", n.id, action_kind(a), i))))
            .collect()
    }
}

/// Kebab-case discriminator string for an [`ActionEntry`], matching the TOML `kind` tag.
fn action_kind(a: &ActionEntry) -> &'static str {
    match a {
        ActionEntry::GitClone => "git-clone",
        ActionEntry::CargoInstall { .. } => "cargo-install",
    }
}