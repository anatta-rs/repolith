//! Core types, traits, manifest parser, and plan computation for repolith.
//!
//! This crate is the foundation of the workspace. It deliberately carries
//! **no async runtime dependency** (no `tokio`, no `futures`) — the
//! parallel execution layer lives one crate up in `repolith-engine`. The
//! split lets downstream consumers (CLI, future TUI/LSP, telemetry) pull
//! the lightweight types here without dragging the runtime tree.
//!
//! # Modules at a glance
//!
//! - [`types`] — value types: `ActionId`, `Sha256`, `BuildEvent`, `Ctx`
//!   (carries the `CancellationToken`), `BuildError`, `ExecMode`.
//! - [`action`] — `trait Action`: `id`, `deps`, `input_hash`, `execute`.
//! - [`source`] — `trait Source`: upstream revision lookup.
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

/// `Source` trait — abstraction over an upstream revision (e.g. git HEAD).
pub mod source;

/// `Cache` trait + `CacheError`. Concrete backends live in dependent crates.
pub mod cache;

/// Layered execution plan with cascading staleness reasons.
pub mod plan;

/// Manifest schema and parser (`repolith.toml`).
pub mod manifest;
