//! `CargoInstall` action — wraps `cargo install --git|--path --locked --force --root <dir>`.
//!
//! The standard install pattern for any Rust binary crate. Like
//! [`crate::git_clone::GitClone`], the spawned `cargo` subprocess is raced
//! against `ctx.cancel` via `tokio::select!` so the orchestrator's
//! `FailFast` mode can short-circuit a long-running build.

use crate::paths::expand_tilde;
use crate::util::{check_status, run_with_cancel};
use async_trait::async_trait;
use repolith_core::action::Action;
use repolith_core::types::{ActionId, BuildError, BuildOutput, Ctx, Sha256};
use sha2::{Digest, Sha256 as ShaHasher};
use std::path::PathBuf;
use tokio::process::Command;

/// Where `cargo install` should pull the crate from.
#[derive(Clone, Debug)]
pub enum CargoSource {
    /// `cargo install --git <url>` (optionally `--branch <branch>`).
    Git {
        /// Remote git URL.
        url: String,
        /// Optional branch override; `None` lets cargo pick the default branch.
        branch: Option<String>,
    },
    /// `cargo install --path <path>` — local source tree.
    Path {
        /// Filesystem path to the crate root (must contain `Cargo.toml`).
        path: PathBuf,
    },
}

/// Action that installs a binary crate via `cargo install`.
///
/// `output_hash` is the SHA-256 of the resulting binary file's content,
/// so a successful re-install with identical inputs yields a stable hash.
pub struct CargoInstall {
    /// Action identifier for the plan.
    pub id: ActionId,
    /// Where the crate source lives.
    pub source: CargoSource,
    /// Crate name to install. When `None`, defaults to the last `::`-separated
    /// segment of [`Self::id`] (the convention used by manifest-derived ids).
    pub crate_name: Option<String>,
    /// Cargo features to enable.
    pub features: Vec<String>,
    /// `--root` argument: cargo writes the binary to `<install_to>/bin/<crate>`.
    pub install_to: PathBuf,
    /// IDs of actions that must complete before this one runs.
    pub deps: Vec<ActionId>,
}

impl CargoInstall {
    fn resolved_crate_name(&self) -> String {
        if let Some(name) = &self.crate_name {
            name.clone()
        } else {
            self.id
                .0
                .rsplit("::")
                .next()
                .unwrap_or(&self.id.0)
                .to_string()
        }
    }
}

#[async_trait]
impl Action for CargoInstall {
    fn id(&self) -> ActionId {
        self.id.clone()
    }

    fn deps(&self) -> Vec<ActionId> {
        self.deps.clone()
    }

    async fn input_hash(&self, ctx: &Ctx) -> Result<Sha256, BuildError> {
        let mut h = ShaHasher::new();
        match &self.source {
            CargoSource::Git { url, branch } => {
                h.update(b"git:");
                h.update(url.as_bytes());
                h.update(b":");
                h.update(branch.as_deref().unwrap_or("").as_bytes());
            }
            CargoSource::Path { path } => {
                h.update(b"path:");
                h.update(path.to_string_lossy().as_bytes());
            }
        }
        for f in &self.features {
            h.update(b":feat:");
            h.update(f.as_bytes());
        }
        h.update(b":crate:");
        h.update(self.resolved_crate_name().as_bytes());

        // Mix in `cargo --version` so a toolchain bump invalidates the cache.
        let mut cmd = Command::new("cargo");
        cmd.arg("--version");
        let v = run_with_cancel(cmd, &ctx.cancel).await?;
        if !v.status.success() {
            return Err(BuildError::UpstreamUnreachable(
                "cargo --version failed".to_string(),
            ));
        }
        h.update(b":cargo:");
        h.update(&v.stdout);
        Ok(Sha256(h.finalize().into()))
    }

    async fn execute(&self, ctx: &Ctx) -> Result<BuildOutput, BuildError> {
        let install_root = expand_tilde(&self.install_to);
        let install_to = install_root.to_str().ok_or_else(|| {
            BuildError::Io(format!("non-utf8 install_to: {}", install_root.display()))
        })?;

        let mut cmd = Command::new("cargo");
        cmd.arg("install");
        match &self.source {
            CargoSource::Git { url, branch } => {
                cmd.args(["--git", url]);
                if let Some(b) = branch {
                    cmd.args(["--branch", b]);
                }
            }
            CargoSource::Path { path } => {
                let p = path.to_str().ok_or_else(|| {
                    BuildError::Io(format!("non-utf8 source path: {}", path.display()))
                })?;
                cmd.args(["--path", p]);
            }
        }
        // Use `--bin <NAME>` instead of the positional `<CRATE>` argument:
        // the positional form filters by *package* name and rejects any
        // mismatch, so packages that expose multiple binaries (or use a
        // package name different from the binary name) can't be installed.
        // `--bin` filters by binary target name and works in both cases.
        let crate_name = self.resolved_crate_name();
        cmd.args(["--bin", &crate_name]);
        if !self.features.is_empty() {
            cmd.args(["--features", &self.features.join(",")]);
        }
        cmd.args(["--locked", "--force", "--root", install_to]);

        check_status(&run_with_cancel(cmd, &ctx.cancel).await?)?;

        // output_hash = sha256(installed binary file content). Fail loudly if
        // the binary cannot be read — silent fallback would mask cargo regressions.
        // Use `install_root` (the tilde-expanded path) to match where cargo
        // actually wrote the binary; reading from `self.install_to` would
        // otherwise look up a literal `~` directory and always fail.
        let bin = install_root.join("bin").join(&crate_name);
        let bytes = std::fs::read(&bin)
            .map_err(|e| BuildError::Io(format!("read installed binary {}: {e}", bin.display())))?;
        let mut h = ShaHasher::new();
        h.update(&bytes);
        Ok(BuildOutput {
            output_hash: Sha256(h.finalize().into()),
            stdout: format!("installed -> {}", bin.display()),
        })
    }
}
