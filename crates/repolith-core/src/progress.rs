//! `ProgressSink` — the action lifecycle, as it happens.
//!
//! Until this existed, `repolith sync` printed nothing while it worked: a cold
//! stack of four repositories meant minutes of a silent terminal followed by a
//! burst of results. The cache was honest about *what* it did, but only after
//! the fact.
//!
//! # Why a sink rather than `tracing`
//!
//! They serve different readers, and the split is deliberate: **the sink and
//! `tracing` never describe the same event.**
//!
//! - The **sink** owns the action lifecycle — layer boundaries, an action
//!   starting, an action ending. It is emitted from exactly one place
//!   ([`crate::action::Action`] execution, driven by the engine's layer loop),
//!   which is what makes "exactly one start and one terminal per action" a
//!   property rather than a hope.
//! - **`tracing`** owns everything the sink cannot see: plan probes, cache
//!   hit/miss, subprocess argv, path containment decisions, federation depth.
//!   It never re-emits a start or a terminal.
//!
//! So `-v` *adds* information instead of repeating it, and a `TracingSink`
//! would be pointless — there is nothing to bridge.
//!
//! # Implementor contract
//!
//! Every method has a no-op default, so a backend only implements what it
//! renders. Two rules bind implementors, and both exist because these methods
//! run **inside the engine's hot loop**, on the same task that is driving the
//! actions:
//!
//! - **Must not block.** A slow sink slows the layer. Writing to a socket
//!   belongs behind a channel, not in these methods.
//! - **Must not panic.** A panic here unwinds the layer. Note that the shipped
//!   [`crate::progress::ProgressSink::on_action_ok`] path makes this mechanical rather than aspirational
//!   by writing with `writeln!` and discarding the `Result` — `println!` panics
//!   on `EPIPE`, and `repolith sync | head` is ordinary usage.
//!
//! # Exactly one terminal per action
//!
//! An action that starts reaches exactly one of [`crate::progress::ProgressSink::on_action_ok`],
//! [`crate::progress::ProgressSink::on_action_failed`] or [`crate::progress::ProgressSink::on_action_cancelled`] — **unless it
//! vanishes**, which happens when a panic unwinds the layer before the future
//! completes. That case is why [`crate::progress::ProgressSink::on_layer_end`] exists: it is emitted
//! from a `Drop` guard, so it fires even during unwinding, and a sink that
//! tracks in-flight actions uses it to reconcile its own bookkeeping.
//!
//! Note what is *not* here: an `on_action_vanished`. The engine cannot know
//! which actions vanished — only a sink's own bookkeeping can, since only it
//! knows which starts it saw without a terminal. Putting that method on the
//! trait would have created an event nobody was in a position to emit
//! correctly.

use crate::types::{ActionId, BuildError};

/// Observer of the action lifecycle during [`crate::plan::Plan`] execution.
///
/// See the [module docs](self) for the implementor contract — the short
/// version is: never block, never panic, and treat [`Self::on_layer_end`] as
/// the point where your bookkeeping must reconcile.
pub trait ProgressSink: Send + Sync {
    /// A layer is about to run. `index` is 1-based, `total` is the layer count.
    ///
    /// `ids` are every action in the layer that will actually run — the plan's
    /// up-to-date entries are already filtered out.
    fn on_layer_start(&self, index: usize, total: usize, ids: &[ActionId]) {
        let _ = (index, total, ids);
    }

    /// An action is about to be awaited.
    ///
    /// Emitted **before** the `await`, not after it resolves. That ordering is
    /// the entire point: an action can run for minutes, and the whole reason
    /// this trait exists is to say so while it happens.
    fn on_action_start(&self, id: &ActionId) {
        let _ = id;
    }

    /// An action succeeded, taking `ms` milliseconds.
    fn on_action_ok(&self, id: &ActionId, ms: u64) {
        let _ = (id, ms);
    }

    /// An action failed on its own merits, after `ms` milliseconds.
    ///
    /// Distinct from [`Self::on_action_cancelled`] on purpose: under
    /// [`crate::types::ExecMode::FailFast`] a single failure cancels its peers,
    /// and reporting those peers as failures would turn one real error into a
    /// screenful of false ones.
    fn on_action_failed(&self, id: &ActionId, err: &BuildError, ms: u64) {
        let _ = (id, err, ms);
    }

    /// An action was cancelled — it did not fail, it was stopped.
    ///
    /// Reached when the context's `CancellationToken` fires: a peer failed
    /// under `FailFast`, or the user pressed Ctrl-C.
    fn on_action_cancelled(&self, id: &ActionId, ms: u64) {
        let _ = (id, ms);
    }

    /// The layer is over — reconcile.
    ///
    /// Emitted from a `Drop` guard in the engine, so it fires on every exit
    /// path including an unwinding panic. A sink tracking in-flight actions
    /// **must** treat this as the point where anything still marked in-flight
    /// never reported a terminal, and clear it. Depending on the terminal
    /// callbacks alone would leak, because an action can vanish without one.
    fn on_layer_end(&self, index: usize) {
        let _ = index;
    }
}

/// The sink used when a caller does not supply one: it does nothing.
///
/// This is what [`crate::plan::Plan`]-executing entry points fall back to, so
/// that adding observation never changes an existing caller's behaviour. A
/// library embedding repolith gets silence by default rather than surprise
/// output on someone else's stderr.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSink;

impl ProgressSink for NoopSink {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records the sequence of callbacks, so tests can assert on ordering
    /// rather than on counts alone.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl ProgressSink for Recorder {
        fn on_action_start(&self, id: &ActionId) {
            self.0
                .lock()
                .expect("not poisoned")
                .push(format!("start:{id}"));
        }
        fn on_action_ok(&self, id: &ActionId, _ms: u64) {
            self.0
                .lock()
                .expect("not poisoned")
                .push(format!("ok:{id}"));
        }
        fn on_layer_end(&self, index: usize) {
            self.0
                .lock()
                .expect("not poisoned")
                .push(format!("layer_end:{index}"));
        }
    }

    fn aid(s: &str) -> ActionId {
        ActionId(s.to_string())
    }

    /// A sink implements only what it renders; the rest must be silently
    /// inert, not a compile error and not a panic.
    #[test]
    fn unimplemented_methods_are_inert() {
        let r = Recorder::default();
        r.on_layer_start(1, 2, &[aid("a")]);
        r.on_action_cancelled(&aid("a"), 5);
        r.on_action_failed(&aid("a"), &BuildError::Cancelled, 5);
        assert!(
            r.0.lock().expect("not poisoned").is_empty(),
            "defaults must do nothing at all"
        );
    }

    #[test]
    fn implemented_methods_record_in_order() {
        let r = Recorder::default();
        r.on_action_start(&aid("a"));
        r.on_action_ok(&aid("a"), 42);
        r.on_layer_end(1);
        assert_eq!(
            *r.0.lock().expect("not poisoned"),
            vec!["start:a", "ok:a", "layer_end:1"]
        );
    }

    /// `NoopSink` is what every existing caller gets. If any default ever
    /// grows a side effect, silence stops being the default and this catches
    /// it — the whole backward-compatibility argument rests on this.
    #[test]
    fn noop_sink_is_usable_as_a_trait_object_and_does_nothing() {
        let sink: &dyn ProgressSink = &NoopSink;
        sink.on_layer_start(1, 1, &[aid("a")]);
        sink.on_action_start(&aid("a"));
        sink.on_action_ok(&aid("a"), 1);
        sink.on_action_failed(&aid("a"), &BuildError::Cancelled, 1);
        sink.on_action_cancelled(&aid("a"), 1);
        sink.on_layer_end(1);
    }
}
