//! Cross-action helpers for spawning subprocesses with cancellation support.
//!
//! Both [`crate::git_clone::GitClone`] and [`crate::cargo_install::CargoInstall`]
//! shell out to external CLIs and need the same two primitives:
//!
//! - [`run_with_cancel`] — race a `tokio::process::Command` against a
//!   [`CancellationToken`], so the orchestrator's `FailFast` mode can
//!   short-circuit a long-running subprocess.
//! - [`check_status`] — turn a non-zero exit status into a
//!   [`BuildError::CommandFailed`] with the trimmed stderr.

use repolith_core::types::BuildError;
use std::process::Output;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Run a `tokio::process::Command` to completion, racing it against `cancel`.
///
/// Returns [`BuildError::Cancelled`] if the token fires first, or
/// [`BuildError::Io`] if the process can't be spawned/waited on.
///
/// **Subprocess cleanup on cancel.** `kill_on_drop(true)` is applied
/// before the `select!` so when the cancel branch wins (and the
/// `cmd.output()` future is dropped), tokio fires `kill(2)` on the
/// child as part of `Drop`. Without this the OS process keeps running
/// until it exits naturally — observable as orphan `git`/`cargo`
/// processes after `Ctrl-C`, and as cache files held by the orphan on
/// the next `repolith sync`.
pub(crate) async fn run_with_cancel(
    mut cmd: Command,
    cancel: &CancellationToken,
) -> Result<Output, BuildError> {
    cmd.kill_on_drop(true);
    tokio::select! {
        result = cmd.output() => result.map_err(|e| BuildError::Io(format!("subprocess: {e}"))),
        () = cancel.cancelled() => Err(BuildError::Cancelled),
    }
}

/// Map a non-zero exit status into [`BuildError::CommandFailed`].
pub(crate) fn check_status(out: &Output) -> Result<(), BuildError> {
    if out.status.success() {
        Ok(())
    } else {
        Err(BuildError::CommandFailed {
            exit_code: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}
