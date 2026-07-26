//! `DockerBuild` action — wraps `docker build -f <dockerfile> -t <tag> -- <context>`.
//!
//! Build-only by design: running containers is process supervision, which
//! repolith explicitly does not do (README "What it is NOT"). Shells out to
//! the host `docker` CLI rather than linking a client library, like every
//! other action in this crate. The spawned subprocess is raced against
//! `ctx.cancel` via the shared `util::run_with_cancel` helper.
//!
//! # Path containment (defense in depth)
//!
//! `dockerfile` and `context` were already validated *lexically* at parse
//! time by `repolith-core` (relative, no `..`, no leading dash). That check
//! cannot see symlinks — `context = "sub"` where `sub` links to `/etc` is
//! lexically clean. `contain_within` closes that hole at run time: both
//! resolved paths are canonicalized and must remain inside the node's
//! canonicalized checkout, or the build fails before `docker` is spawned.

use crate::util::{check_status, run_with_cancel};
use async_trait::async_trait;
use repolith_core::action::Action;
use repolith_core::paths::contain_within;
use repolith_core::types::{ActionId, BuildError, BuildOutput, Ctx, Sha256};
use sha2::{Digest, Sha256 as ShaHasher};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Action that builds a docker image from the node's checkout.
///
/// `output_hash` is the SHA-256 of the built image id (`docker image
/// inspect --format {{.Id}}`), so an identical rebuild yields a stable
/// hash and a genuinely different image changes it.
pub struct DockerBuild {
    /// Action identifier for the plan.
    pub id: ActionId,
    /// Image tag (`-t`). Validated by the manifest layer.
    pub tag: String,
    /// Dockerfile path relative to the build context. `None` = `Dockerfile`.
    pub dockerfile: Option<PathBuf>,
    /// Build context relative to [`Self::node_path`]. `None` = the node path itself.
    pub context: Option<PathBuf>,
    /// The node's checkout directory — the containment boundary for
    /// `dockerfile` and `context`.
    pub node_path: PathBuf,
    /// IDs of actions that must complete before this one runs.
    pub deps: Vec<ActionId>,
}

impl DockerBuild {
    /// Canonicalized `(context_dir, dockerfile_path)`, both proven to be
    /// inside the node's checkout.
    fn resolved_paths(&self) -> Result<(PathBuf, PathBuf), BuildError> {
        let context_rel = self.context.as_deref().unwrap_or(Path::new("."));
        let context = contain_within(&self.node_path, context_rel)?;
        let dockerfile_rel = self
            .dockerfile
            .as_deref()
            .unwrap_or(Path::new("Dockerfile"));
        // The dockerfile is relative to the *context*, but must stay inside
        // the *checkout* — a context subdir must not reach a sibling via
        // symlink even if that sibling is inside the context's parent.
        let dockerfile = contain_within(&self.node_path, &context_rel.join(dockerfile_rel))?;
        Ok((context, dockerfile))
    }
}

#[async_trait]
impl Action for DockerBuild {
    fn id(&self) -> ActionId {
        self.id.clone()
    }

    fn deps(&self) -> Vec<ActionId> {
        self.deps.clone()
    }

    async fn input_hash(&self, ctx: &Ctx) -> Result<Sha256, BuildError> {
        let mut h = ShaHasher::new();
        h.update(b"docker:tag:");
        h.update(self.tag.as_bytes());

        // Pre-clone grace: a sibling git-clone earlier in the same node may
        // not have materialized the checkout yet at plan time. Hash a
        // deterministic marker instead of failing the whole plan; the
        // action is stale anyway (NoCachedBuild / UpstreamMoved cascade),
        // runs after the clone, and the next sync re-hashes real bytes
        // (one extra, harmless rebuild). `execute` stays strict.
        if !self.node_path.exists() {
            h.update(b":pre-clone");
            return Ok(Sha256(h.finalize().into()));
        }
        let (context, dockerfile) = self.resolved_paths()?;

        // Dockerfile content — the primary build input.
        let df_bytes = tokio::fs::read(&dockerfile).await.map_err(|e| {
            BuildError::Io(format!("read dockerfile {}: {e}", dockerfile.display()))
        })?;
        h.update(b":dockerfile:");
        h.update(&df_bytes);

        // Checkout state: HEAD when the context is a git worktree, a fixed
        // marker otherwise (path-only nodes). A non-git context therefore
        // re-runs only when the Dockerfile, tag, or docker version changes —
        // documented trade-off, same family as cargo-install's source hash.
        let mut head = Command::new("git");
        head.args(["-C"]).arg(&context).args(["rev-parse", "HEAD"]);
        let head_out = run_with_cancel(head, &ctx.cancel).await?;
        h.update(b":checkout:");
        if head_out.status.success() {
            h.update(&head_out.stdout);
        } else {
            h.update(b"no-git");
        }

        // Mix in `docker --version` so an engine bump invalidates the cache.
        // Its absence is a hard, actionable error — not a silent cache hit.
        let mut ver = Command::new("docker");
        ver.arg("--version");
        let v = run_with_cancel(ver, &ctx.cancel).await?;
        if !v.status.success() {
            return Err(BuildError::UpstreamUnreachable(
                "docker --version failed — is the docker CLI installed and on PATH?".to_string(),
            ));
        }
        h.update(b":docker:");
        h.update(&v.stdout);
        Ok(Sha256(h.finalize().into()))
    }

    async fn execute(&self, ctx: &Ctx) -> Result<BuildOutput, BuildError> {
        let (context, dockerfile) = self.resolved_paths()?;
        let context_str = context.to_str().ok_or_else(|| {
            BuildError::Io(format!("non-utf8 context path: {}", context.display()))
        })?;
        let dockerfile_str = dockerfile.to_str().ok_or_else(|| {
            BuildError::Io(format!(
                "non-utf8 dockerfile path: {}",
                dockerfile.display()
            ))
        })?;

        let mut cmd = Command::new("docker");
        cmd.args([
            "build",
            "-f",
            dockerfile_str,
            "-t",
            &self.tag,
            "--",
            context_str,
        ]);
        check_status(&run_with_cancel(cmd, &ctx.cancel).await?)?;

        // output_hash = sha256(image id). `docker image inspect` right after
        // a successful build failing would mean the daemon lied — fail loudly.
        let mut inspect = Command::new("docker");
        inspect.args(["image", "inspect", "--format", "{{.Id}}", "--", &self.tag]);
        let out = run_with_cancel(inspect, &ctx.cancel).await?;
        if !out.status.success() {
            return Err(BuildError::Io(format!(
                "docker image inspect {} failed after build: {}",
                self.tag,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let image_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if image_id.is_empty() {
            return Err(BuildError::Io(format!(
                "docker image inspect {} returned an empty id",
                self.tag
            )));
        }
        let mut h = ShaHasher::new();
        h.update(image_id.as_bytes());
        Ok(BuildOutput {
            output_hash: Sha256(h.finalize().into()),
            stdout: format!("built {} -> {image_id}", self.tag),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contain_within_accepts_inner_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let got = contain_within(dir.path(), Path::new("sub")).unwrap();
        assert!(got.ends_with("sub"));
    }

    #[test]
    fn contain_within_rejects_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(contain_within(dir.path(), Path::new("absent")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn contain_within_rejects_symlink_escape() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("evil")).unwrap();
        let err = contain_within(dir.path(), Path::new("evil")).unwrap_err();
        assert!(err.to_string().contains("escapes"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn contain_within_rejects_symlinked_file_escape() {
        // A symlink to a *file* outside (e.g. dockerfile -> /etc/passwd).
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, b"x").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&secret, dir.path().join("Dockerfile")).unwrap();
        let err = contain_within(dir.path(), Path::new("Dockerfile")).unwrap_err();
        assert!(err.to_string().contains("escapes"), "got: {err}");
    }
}
