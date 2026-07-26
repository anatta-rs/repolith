//! Async runtime for repolith — the engine that turns a `Plan` into `BuildEvent`s.
//!
//! `repolith-core` stays a pure types/traits crate. This crate adds the
//! parallel layer executor on top, with cancellation, semaphore-based
//! concurrency limiting, and `FailFast`/`KeepGoing` semantics.

/// Orchestrator + builder + execution errors.
pub mod orchestrator;

/// Federation — `kind = "repolith"` executes a child stack's manifest as a
/// nested plan (orchestrator-of-orchestrators).
pub mod federation;
