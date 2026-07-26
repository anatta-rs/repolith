//! `CargoInstall` action — wraps `cargo install --git|--path --locked --force --root <dir>`.
//!
//! The standard install pattern for any Rust binary crate. Like
//! [`crate::git_clone::GitClone`], the spawned `cargo` subprocess is raced
//! against `ctx.cancel` via `tokio::select!` so the orchestrator's
//! `FailFast` mode can short-circuit a long-running build.

use crate::paths::expand_tilde;
use crate::source_hash::{platform_tag, tree_digest};
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
    /// Binary target to install (`--bin`). When `None`, defaults to the last
    /// `::`-separated segment of [`Self::id`] (the convention used by
    /// manifest-derived ids).
    pub crate_name: Option<String>,
    /// Package to select from the source, when it holds more than one
    /// (cargo's positional `[CRATE]` argument). `None` leaves the choice to
    /// cargo — see the argv comment in [`Action::execute`] for why this has
    /// no implicit default.
    pub package: Option<String>,
    /// Cargo profile (`--profile`). `None` leaves cargo's default, release.
    pub profile: Option<String>,
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

    /// Where cargo writes the binary: `<install_to>/bin/<crate><EXE_SUFFIX>`.
    /// Single source of truth for `execute` (which reads it back to hash it)
    /// and `output_present` (which only checks it exists).
    fn installed_bin(&self) -> PathBuf {
        let bin_name = format!(
            "{}{}",
            self.resolved_crate_name(),
            std::env::consts::EXE_SUFFIX
        );
        expand_tilde(&self.install_to).join("bin").join(bin_name)
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
                // The tree's *content*, not its name. Hashing the path
                // string alone made every local edit invisible: the hash
                // never moved, so `sync` reported `up to date` while the
                // installed binary was stale (issue #73).
                let expanded = expand_tilde(path);
                h.update(b"path:");
                if expanded.exists() {
                    h.update(tree_digest(&expanded)?.0);
                } else {
                    // Pre-clone grace: a sibling `git-clone` earlier in the
                    // same node materializes this tree, but planning runs
                    // before any execution. Hash a deterministic marker
                    // rather than failing the whole plan — the action is
                    // stale regardless, runs after the clone, and the next
                    // sync digests the real content. Same treatment as
                    // `docker` and federation.
                    h.update(b"pre-clone");
                }
            }
        }
        platform_tag(&mut h);
        for f in &self.features {
            h.update(b":feat:");
            h.update(f.as_bytes());
        }
        h.update(b":crate:");
        h.update(self.resolved_crate_name().as_bytes());
        // Two nodes over the same source that differ only by which package
        // they select must not share a hash, or the second silently inherits
        // the first's cached Success.
        h.update(b":pkg:");
        h.update(self.package.as_deref().unwrap_or("").as_bytes());
        // The profile changes the artifact but not where it lands, so
        // `output_present` cannot tell release from debug. Without this the
        // cache would report `up to date` after a profile switch, leaving
        // the other profile's binary installed.
        h.update(b":profile:");
        h.update(self.profile.as_deref().unwrap_or("").as_bytes());

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
        // Two orthogonal selectors, and both are needed:
        //
        // - the positional `[CRATE]` picks the *package* out of the source
        //   (cargo has no `--package` for `install`);
        // - `--bin` picks the binary *target* within it.
        //
        // Only `--bin` used to be passed, because the positional alone
        // rejects a package whose name differs from the binary's. That fix
        // stands — but it left multi-package git repos unreachable, since
        // cargo resolves the package first and refuses to guess between
        // several (issue #77). Passing the positional only when `package` is
        // set keeps the old behavior byte-for-byte for every existing
        // manifest.
        if let Some(pkg) = &self.package {
            cmd.arg(pkg);
        }
        let crate_name = self.resolved_crate_name();
        cmd.args(["--bin", &crate_name]);
        if let Some(profile) = &self.profile {
            cmd.args(["--profile", profile]);
        }
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
        //
        // Windows binaries have `.exe` suffix; `EXE_SUFFIX` is `""` on Unix.
        // `tokio::fs::read` releases the async worker for the (potentially
        // multi-MB) read instead of blocking it like `std::fs::read` did.
        let bin = self.installed_bin();
        let bytes = tokio::fs::read(&bin)
            .await
            .map_err(|e| BuildError::Io(format!("read installed binary {}: {e}", bin.display())))?;
        let mut h = ShaHasher::new();
        h.update(&bytes);
        Ok(BuildOutput {
            output_hash: Sha256(h.finalize().into()),
            stdout: format!("installed -> {}", bin.display()),
        })
    }

    /// The installed binary must still be on this machine. A cached
    /// `Success` alone proves only that *some* machine built it.
    async fn output_present(&self, _ctx: &Ctx) -> bool {
        self.installed_bin().is_file()
    }
}
