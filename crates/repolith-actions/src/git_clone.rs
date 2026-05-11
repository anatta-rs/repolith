//! `GitClone` action — fetches a git repository into a local path.
//!
//! Shells out to the `git` CLI rather than linking `git2` (no C bindings,
//! faster compile, and `git` is a hard requirement on every dev/CI host
//! anyway). Every spawned subprocess is raced against the [`Ctx::cancel`]
//! token via `tokio::select!` so the orchestrator's `FailFast` mode can
//! short-circuit a long-running clone.
//!
//! [`Ctx::cancel`]: repolith_core::types::Ctx::cancel

use crate::util::{check_status, run_with_cancel};
use async_trait::async_trait;
use repolith_core::action::Action;
use repolith_core::types::{ActionId, BuildError, BuildOutput, Ctx, Sha256};
use sha2::{Digest, Sha256 as ShaHasher};
use std::path::PathBuf;
use tokio::process::Command;

/// Action that mirrors a remote git repository into a local working tree.
///
/// On first execution, performs `git clone <url> <path>`. On subsequent
/// executions, performs `git -C <path> fetch origin` followed by
/// `git -C <path> reset --hard FETCH_HEAD`. The `output_hash` is the
/// SHA-256 of the freshly-checked-out HEAD commit's SHA-1.
pub struct GitClone {
    /// Action identifier for the plan.
    pub id: ActionId,
    /// Remote git URL to clone from.
    pub repo_url: String,
    /// Local destination path.
    pub path: PathBuf,
    /// IDs of actions that must complete before this one runs.
    pub deps: Vec<ActionId>,
}

#[async_trait]
impl Action for GitClone {
    fn id(&self) -> ActionId {
        self.id.clone()
    }

    fn deps(&self) -> Vec<ActionId> {
        self.deps.clone()
    }

    async fn input_hash(&self, ctx: &Ctx) -> Result<Sha256, BuildError> {
        // `git ls-remote <url> HEAD` returns one line: `<sha1-hex>\tHEAD`.
        let mut cmd = Command::new("git");
        cmd.args(["ls-remote", &self.repo_url, "HEAD"]);
        let out = run_with_cancel(cmd, &ctx.cancel).await?;
        if !out.status.success() {
            return Err(BuildError::UpstreamUnreachable(format!(
                "git ls-remote {}: {}",
                self.repo_url,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let sha1 = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        Ok(hash_url_and_sha(&self.repo_url, &sha1))
    }

    async fn execute(&self, ctx: &Ctx) -> Result<BuildOutput, BuildError> {
        let path_str = self
            .path
            .to_str()
            .ok_or_else(|| BuildError::Io(format!("non-utf8 path: {:?}", self.path)))?;

        if self.path.join(".git").is_dir() {
            // Refresh existing clone: fetch + hard reset.
            let mut fetch = Command::new("git");
            fetch.args(["-C", path_str, "fetch", "origin", "--quiet"]);
            check_status(&run_with_cancel(fetch, &ctx.cancel).await?)?;

            let mut reset = Command::new("git");
            reset.args(["-C", path_str, "reset", "--hard", "FETCH_HEAD", "--quiet"]);
            check_status(&run_with_cancel(reset, &ctx.cancel).await?)?;
        } else {
            // First clone — create parent dir if needed.
            if let Some(parent) = self.path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| BuildError::Io(format!("create_dir_all: {e}")))?;
            }
            let mut clone = Command::new("git");
            clone.args(["clone", "--quiet", &self.repo_url, path_str]);
            check_status(&run_with_cancel(clone, &ctx.cancel).await?)?;
        }

        // Resolve current HEAD for output_hash + diagnostic stdout.
        let mut head = Command::new("git");
        head.args(["-C", path_str, "rev-parse", "HEAD"]);
        let head_out = run_with_cancel(head, &ctx.cancel).await?;
        if !head_out.status.success() {
            return Err(BuildError::CommandFailed {
                exit_code: head_out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&head_out.stderr).trim().to_string(),
            });
        }
        let sha1 = String::from_utf8_lossy(&head_out.stdout).trim().to_string();
        let mut h = ShaHasher::new();
        h.update(sha1.as_bytes());
        Ok(BuildOutput {
            output_hash: Sha256(h.finalize().into()),
            stdout: format!("HEAD {sha1}"),
        })
    }
}

/// Hash a `(url, sha1-hex)` pair into a `Sha256` for use as `input_hash`.
/// Mixing the URL prevents collisions across different mirrors of the
/// same commit.
fn hash_url_and_sha(url: &str, sha1: &str) -> Sha256 {
    let mut h = ShaHasher::new();
    h.update(url.as_bytes());
    h.update(b":");
    h.update(sha1.as_bytes());
    Sha256(h.finalize().into())
}

