//! `CliProgress` — what `repolith sync` prints while it works.
//!
//! # The single-writer rule
//!
//! `CliProgress` is the **only** thing that writes to stdout during a sync.
//! The heartbeat holds an `Arc<CliProgress>` and writes *through* it; the
//! final summary goes through it too. One type, one `Mutex<Stdout>`, so lines
//! serialise instead of interleaving.
//!
//! It never holds two locks at once: the in-flight map is snapshotted, the
//! guard dropped, and only then is anything written. That makes deadlock
//! impossible by construction rather than by remembering a lock order.
//!
//! # Two rules that are not style preferences
//!
//! **`writeln!` with the `Result` discarded, never `println!`.** `println!`
//! panics when the write fails, Rust ignores `SIGPIPE`, and `repolith sync |
//! head` is ordinary usage — so `println!` would panic on `EPIPE` and poison
//! the stdout mutex. Discarding the error makes the sink's "must not panic"
//! contract mechanical instead of aspirational.
//!
//! **Poison is handled per operation, not uniformly.** Recovering a poisoned
//! lock to *discard* the map is harmless — we throw it away. Recovering one to
//! *read* from it would mean displaying state we know to be inconsistent, so
//! the tick stays silent instead. Same primitive, opposite answers, because
//! the operations have opposite requirements.

use repolith_core::progress::ProgressSink;
use repolith_core::types::{ActionId, BuildError};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// How often the heartbeat reports what is still running.
const TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// Renders the action lifecycle to stdout, and owns the in-flight bookkeeping
/// the heartbeat reads from.
pub struct CliProgress {
    out: Mutex<std::io::Stdout>,
    in_flight: Mutex<HashMap<ActionId, Instant>>,
}

impl CliProgress {
    pub fn new() -> Self {
        Self {
            out: Mutex::new(std::io::stdout()),
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    /// Write one line, swallowing I/O errors.
    ///
    /// On a poisoned lock we recover *and* emit a leading newline: the thread
    /// that panicked may have left a half-written line behind, and appending
    /// to it would compound the damage.
    fn line(&self, s: &str) {
        match self.out.lock() {
            Ok(mut o) => {
                let _ = writeln!(o, "{s}");
            }
            Err(poisoned) => {
                let mut o = poisoned.into_inner();
                let _ = writeln!(o, "\n{s}");
            }
        }
    }

    /// Snapshot of what is running, oldest first. `None` when the lock is
    /// poisoned — see the module docs on why reading is not recovered.
    fn snapshot(&self) -> Option<Vec<(ActionId, std::time::Duration)>> {
        let map = self.in_flight.lock().ok()?;
        let mut v: Vec<_> = map
            .iter()
            .map(|(id, t)| (id.clone(), t.elapsed()))
            .collect();
        drop(map);
        // Longest-running first: on a slow layer that is the action the
        // reader is actually waiting on.
        v.sort_by_key(|(_, elapsed)| std::cmp::Reverse(*elapsed));
        Some(v)
    }

    /// One heartbeat tick. Silent when nothing is running.
    fn tick(&self) {
        let Some(running) = self.snapshot() else {
            return;
        };
        for (id, elapsed) in running {
            self.line(&format!(
                "  … still running: {id} ({})",
                human_ms(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            ));
        }
    }

    /// Final summary: counts, plus every failure repeated.
    ///
    /// Deliberately not a replay of each event — the per-action lines were
    /// already printed live, and re-listing them would double every success.
    pub fn summary(&self, events: &[repolith_core::types::BuildEvent]) {
        use repolith_core::types::BuildEvent;
        let (mut ok, mut failed) = (0usize, 0usize);
        for e in events {
            match e {
                BuildEvent::Success { .. } => ok += 1,
                BuildEvent::Failed { .. } => failed += 1,
            }
        }
        if ok + failed == 0 {
            return;
        }
        self.line(&format!("{ok} ok, {failed} failed"));
        for e in events {
            if let BuildEvent::Failed { id, error, .. } = e {
                self.line(&format!("  ✗ {id}: {}", brief(error)));
            }
        }
    }
}

impl Default for CliProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressSink for CliProgress {
    fn on_layer_start(&self, index: usize, total: usize, ids: &[ActionId]) {
        let n = ids.len();
        let plural = if n == 1 { "" } else { "s" };
        self.line(&format!("layer {index}/{total} — {n} action{plural}"));
    }

    fn on_action_start(&self, id: &ActionId) {
        if let Ok(mut m) = self.in_flight.lock() {
            m.insert(id.clone(), Instant::now());
        }
        self.line(&format!("  → {id}"));
    }

    fn on_action_ok(&self, id: &ActionId, ms: u64) {
        self.forget(id);
        self.line(&format!("  ✓ {id}   {}", human_ms(ms)));
    }

    fn on_action_failed(&self, id: &ActionId, err: &BuildError, ms: u64) {
        self.forget(id);
        self.line(&format!("  ✗ {id}   {} — {}", human_ms(ms), brief(err)));
    }

    fn on_action_cancelled(&self, id: &ActionId, ms: u64) {
        self.forget(id);
        self.line(&format!("  ⊘ {id}   {} — cancelled", human_ms(ms)));
    }

    fn on_layer_end(&self, _index: usize) {
        // Reconcile. Anything still here started and never reported a
        // terminal — it vanished, which happens when a panic unwinds the
        // layer. Depending on the terminal callbacks alone would leak, and a
        // silent purge would hide the very bug this catches, so say which.
        let vanished: Vec<ActionId> = match self.in_flight.lock() {
            Ok(mut m) => m.drain().map(|(id, _)| id).collect(),
            // Discarding an inconsistent map is safe — we throw it away.
            Err(p) => p.into_inner().drain().map(|(id, _)| id).collect(),
        };
        for id in vanished {
            self.line(&format!("  ⚠ {id} — vanished without reporting"));
        }
    }
}

impl CliProgress {
    /// Remove from the in-flight map **before** writing.
    ///
    /// The order matters: a panic between the remove and the write can no
    /// longer leave a stale entry that `on_layer_end` would then report as
    /// vanished. Inverting these two lines reintroduces a class of lying
    /// reports that no amount of extra state could distinguish.
    fn forget(&self, id: &ActionId) {
        if let Ok(mut m) = self.in_flight.lock() {
            m.remove(id);
        }
    }
}

/// Background task that reports what is still running, every [`TICK`].
///
/// Cancelled through a [`CancellationToken`] rather than `JoinHandle::abort`:
/// abort can kill the task mid-`writeln!` and leave a truncated line, which is
/// exactly the output this feature exists to clean up. `Drop` is the safety
/// net for paths that return early; [`Self::stop`] is the ordered shutdown.
pub struct Heartbeat {
    token: CancellationToken,
    handle: Option<JoinHandle<()>>,
}

impl Heartbeat {
    pub fn spawn(progress: Arc<CliProgress>) -> Self {
        let token = CancellationToken::new();
        let child = token.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // `biased` with the token first: select! polls branches in
                    // random order by default, so a tick could win the race
                    // against a cancel that has already fired.
                    biased;
                    () = child.cancelled() => return,
                    () = tokio::time::sleep(TICK) => progress.tick(),
                }
            }
        });
        Self {
            token,
            handle: Some(handle),
        }
    }

    /// Ordered shutdown: cancel, then wait for an in-flight write to finish.
    ///
    /// `cancel()` alone makes the loop exit, but does not wait — without the
    /// join, a tick already inside `writeln!` can land between two lines of
    /// the summary.
    pub async fn stop(mut self) {
        self.token.cancel();
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        // Safety net for every path that does not call `stop` — an early
        // `?`, or a panic. Cannot await here, so the task is left to observe
        // the token on its next poll.
        self.token.cancel();
    }
}

/// One short line for a build error.
///
/// Same 72-character cap `ChangeReason`'s `Display` uses, for the same reason:
/// a tool's first stderr line is often a warning, and the useful part rarely
/// survives a terminal wrap. The whole error stays reachable — it is in the
/// cache, and `repolith status <filter>` prints it untruncated.
fn brief(err: &BuildError) -> String {
    let s = err.to_string();
    let first = s.lines().next().unwrap_or("");
    let cut: String = first.chars().take(72).collect();
    if cut.chars().count() < first.chars().count() {
        format!("{cut}…")
    } else {
        cut
    }
}

/// A duration a human reads at a glance. Integer arithmetic throughout — a
/// float cast would buy nothing here but a precision-loss lint.
fn human_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{}.{} s", ms / 1000, (ms % 1000) / 100)
    } else {
        format!("{} min {} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aid(s: &str) -> ActionId {
        ActionId(s.to_string())
    }

    #[test]
    fn durations_read_like_durations() {
        assert_eq!(human_ms(999), "999 ms");
        assert_eq!(human_ms(26_800), "26.8 s");
        assert_eq!(human_ms(161_000), "2 min 41 s");
    }

    #[test]
    fn a_terminal_clears_the_in_flight_entry() {
        let p = CliProgress::new();
        p.on_action_start(&aid("a"));
        assert_eq!(p.snapshot().expect("not poisoned").len(), 1);
        p.on_action_ok(&aid("a"), 1);
        assert!(
            p.snapshot().expect("not poisoned").is_empty(),
            "a reported terminal must leave nothing behind"
        );
    }

    /// The invariant the whole `LayerGuard` mechanism exists for.
    #[test]
    fn on_layer_end_leaves_the_map_empty_even_with_a_vanished_action() {
        let p = CliProgress::new();
        p.on_action_start(&aid("gone"));
        p.on_action_start(&aid("done"));
        p.on_action_ok(&aid("done"), 1);
        // `gone` never reported a terminal — it vanished.
        p.on_layer_end(1);
        assert!(
            p.snapshot().expect("not poisoned").is_empty(),
            "the layer boundary must reconcile whatever the terminals missed"
        );
    }

    /// The heartbeat must say nothing when nothing runs, or a quiet sync
    /// becomes a stream of empty reports.
    #[test]
    fn a_tick_with_nothing_in_flight_is_silent() {
        let p = CliProgress::new();
        assert!(p.snapshot().expect("not poisoned").is_empty());
        p.tick(); // must not panic, must print nothing
    }

    #[test]
    fn the_snapshot_is_ordered_oldest_first() {
        let p = CliProgress::new();
        p.on_action_start(&aid("older"));
        std::thread::sleep(std::time::Duration::from_millis(12));
        p.on_action_start(&aid("newer"));
        let snap = p.snapshot().expect("not poisoned");
        assert_eq!(snap[0].0, aid("older"), "longest-running first: {snap:?}");
    }
}
