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
use std::process::{Output, Stdio};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Grace period between SIGTERM and SIGKILL when escalating a cancel.
/// 250 ms is enough for well-behaved children (git, cargo) to flush
/// stdio buffers and exit cleanly, and short enough not to delay the
/// orchestrator's overall shutdown.
#[cfg(unix)]
const SIGTERM_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// Run a `tokio::process::Command` to completion, racing it against `cancel`.
///
/// Returns [`BuildError::Cancelled`] if the token fires first, or
/// [`BuildError::Io`] if the process can't be spawned/waited on.
///
/// **Subprocess cleanup on cancel.** On Unix the child is placed in its
/// own process group via [`std::os::unix::process::CommandExt::process_group`].
/// When `cancel` fires, the group receives `SIGTERM`; after a short grace
/// the group receives `SIGKILL`. This reaps grandchildren (e.g. cargo's
/// `rustc`/linker subprocesses) which `kill_on_drop` alone misses.
/// `kill_on_drop(true)` is kept as the fallback so the immediate child is
/// killed even when the cancel branch never fires (e.g. the future is
/// dropped because the orchestrator panics).
///
/// On non-Unix platforms only `kill_on_drop(true)` is applied; the
/// process-group machinery is a no-op.
pub(crate) async fn run_with_cancel(
    mut cmd: Command,
    cancel: &CancellationToken,
) -> Result<Output, BuildError> {
    cmd.kill_on_drop(true);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // On Unix, process_group(0) makes the child the leader of a new group
    // whose pgid equals its pid — captured below so a cancel can signal
    // the whole tree (children + grandchildren), not just the direct
    // child that `kill_on_drop` knows about.
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd
        .spawn()
        .map_err(|e| BuildError::Io(format!("spawn: {e}")))?;
    #[cfg(unix)]
    let pgid = child.id().and_then(|p| i32::try_from(p).ok());
    let wait = child.wait_with_output();
    tokio::select! {
        result = wait => result.map_err(|e| BuildError::Io(format!("subprocess: {e}"))),
        () = cancel.cancelled() => {
            #[cfg(unix)]
            if let Some(pgid) = pgid {
                kill_group(pgid);
            }
            Err(BuildError::Cancelled)
        }
    }
}

/// SIGTERM the process group, wait [`SIGTERM_GRACE`], then SIGKILL if any
/// member is still alive. Errors are swallowed: the only failure mode is
/// "child already exited", which is the desired end state anyway.
#[cfg(unix)]
fn kill_group(pgid: i32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let pg = Pid::from_raw(pgid);
    let _ = killpg(pg, Signal::SIGTERM);
    tokio::spawn(async move {
        tokio::time::sleep(SIGTERM_GRACE).await;
        let _ = killpg(pg, Signal::SIGKILL);
    });
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A `sh -c 'sleep 30'` would block for 30 s if cancel didn't reach
    /// the subprocess. We assert the function returns `Cancelled` in well
    /// under that — the only way it can is the SIGTERM/SIGKILL path firing
    /// against the process group.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_reaps_subprocess_fast() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move { run_with_cancel(cmd, &cancel_clone).await });

        // Let the subprocess actually start.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let start = std::time::Instant::now();
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), handle)
            .await
            .expect("must finish within 3s")
            .expect("task panic");

        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "cancel took too long: {:?}",
            start.elapsed()
        );
        assert!(matches!(result, Err(BuildError::Cancelled)));
    }

    /// Spawn a shell that forks a `sleep` grandchild and writes its pid
    /// to a tempfile. After cancel + grace, verify the grandchild pid is
    /// gone — this is what `kill_on_drop` alone cannot do, since it only
    /// signals the immediate child (`sh`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_reaps_grandchild() {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("sleep.pid");
        let script = format!("sleep 30 & echo $! > {} ; wait", pidfile.to_str().unwrap());
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &script]);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move { run_with_cancel(cmd, &cancel_clone).await });

        // Wait until the pidfile appears (grandchild started).
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if pidfile.exists()
                && std::fs::metadata(&pidfile).is_ok_and(|m| m.len() > 0)
            {
                break;
            }
        }
        let pid_text = std::fs::read_to_string(&pidfile).expect("pidfile must be written");
        let grandchild_pid: i32 = pid_text.trim().parse().expect("pid is a number");

        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), handle).await;

        // Wait past SIGTERM_GRACE to let SIGKILL land, then verify the
        // grandchild is gone. `kill(pid, 0)` returns ESRCH if there is no
        // such process.
        tokio::time::sleep(SIGTERM_GRACE + std::time::Duration::from_millis(300)).await;
        let probe = kill(Pid::from_raw(grandchild_pid), None);
        assert!(
            matches!(probe, Err(Errno::ESRCH)),
            "grandchild pid {grandchild_pid} still alive after cancel + grace: {probe:?}"
        );
    }
}
