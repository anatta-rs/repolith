//! Async runtime for repolith — the engine that turns a [`Plan`] into [`BuildEvent`]s.
//!
//! [`Plan`]: repolith_core::plan::Plan
//! [`BuildEvent`]: repolith_core::types::BuildEvent
//!
//! `repolith-core` stays a pure types/traits crate. This crate adds the
//! parallel layer executor on top, with cancellation, semaphore-based
//! concurrency limiting, and `FailFast`/`KeepGoing` semantics.

/// Orchestrator + builder + execution errors.
pub mod orchestrator;
