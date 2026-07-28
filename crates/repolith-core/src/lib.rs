//! Core types, traits, manifest parser, and plan computation for repolith.
//!
//! This crate is the foundation of the workspace. It carries **no `tokio`
//! runtime dependency** — only lightweight runtime-agnostic combinators
//! from `futures` (used inside `Plan::compute` to fan probes out
//! concurrently). The orchestrator + executor live one crate up in
//! `repolith-engine`, which pulls in `tokio`. The split lets downstream
//! consumers (CLI, future TUI/LSP, telemetry) pull the lightweight types
//! here without dragging the runtime tree.
//!
//! # Modules at a glance
//!
//! - [`types`] — value types: `ActionId`, `Sha256`, `BuildEvent`, `Ctx`
//!   (carries the `CancellationToken`), `BuildError`, `ExecMode`.
//! - [`action`] — `trait Action`: `id`, `deps`, `input_hash`, `execute`.
//! - [`cache`] — `trait Cache` + `CacheError`. Implementations live in
//!   `repolith-cache`.
//! - [`plan`] — `Plan::compute` (Kahn topological sort + cascading
//!   staleness via `ChangeReason`).
//! - [`manifest`] — `Manifest::from_toml` (parse + validate
//!   `repolith.toml`).
//!
//! # Typical use
//!
//! ```ignore
//! use repolith_core::manifest::Manifest;
//!
//! let toml = std::fs::read_to_string("repolith.toml")?;
//! let manifest = Manifest::from_toml(&toml)?;
//! for id in manifest.action_ids() {
//!     println!("{id}");
//! }
//! ```
//!
//! Actually executing the plan happens through `repolith-engine`'s
//! `Orchestrator`, which consumes the types defined here.

/// Value types used across the workspace.
pub mod types;

/// `Action` trait — one unit of orchestrated work.
pub mod action;

/// `Cache` trait + `CacheError`. Concrete backends live in dependent crates.
pub mod cache;

/// Layered execution plan with cascading staleness reasons.
pub mod plan;

/// `ProgressSink` — observe the action lifecycle while it happens, rather
/// than reading about it once the plan has settled.
pub mod progress;

/// Manifest schema and parser (`repolith.toml`).
pub mod manifest;

/// Run-time path containment (`contain_within`) — shared by actions that
/// resolve user-supplied relative paths (docker, federation).
pub mod paths;
