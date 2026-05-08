//! Core types and traits for repolith.

/// Contains all the fundamental data structures used across the workspace.
pub mod types;

/// represent an actions to perform
pub mod action;
/// source of code, usually a git
pub mod source;

/// Cache trait — abstract storage for build events. Impls live in dependent crates.
pub mod cache;

/// Build plan — immutable DAG snapshot with staleness reasons.
pub mod plan;

/// Manifest schema and parser (`repolith.toml`).
pub mod manifest;