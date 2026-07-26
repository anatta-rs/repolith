//! Concrete [`Action`](repolith_core::action::Action) implementations
//! for repolith.
//!
//! Each backend lives behind its own cargo feature so consumers only
//! pull the dependencies they actually need:
//!
//! | Feature  | Module                          | What it does                                         |
//! |----------|---------------------------------|------------------------------------------------------|
//! | `git`    | [`git_clone::GitClone`]         | shells out to `git clone` / `git fetch`              |
//! | `cargo`  | [`cargo_install::CargoInstall`] | shells out to `cargo install --bin <name> --locked`  |
//! | `docker` | [`docker::DockerBuild`]         | shells out to `docker build -f … -t … -- <context>`  |
//!
//! Every action shells out to an external CLI rather than linking the
//! corresponding library (no `git2`, no `cargo` as a crate). Two reasons:
//! faster compile times, and the host already has the CLIs by hypothesis
//! (you can't run `repolith sync` without `git` and `cargo` on `PATH`).
//!
//! Every spawned subprocess is raced against `ctx.cancel` via the
//! shared `util::run_with_cancel` helper so the orchestrator's
//! `FailFast` mode can short-circuit a long-running clone or build.
//! New actions that shell out should go through the same helper.
//!
//! See [`CONTRIBUTING.md`](https://github.com/anatta-rs/repolith/blob/main/CONTRIBUTING.md)
//! for the recipe to add a new action.

#[cfg(any(feature = "git", feature = "cargo", feature = "docker"))]
pub(crate) mod util;

/// Path utilities (tilde expansion). Public so the manifest factory in the
/// CLI can apply the same expansion to `node.path` that `cargo_install`
/// already applies to `install_to`.
pub mod paths;

/// `GitClone` action — mirror a remote git repository into a local path.
#[cfg(feature = "git")]
pub mod git_clone;

/// `CargoInstall` action — `cargo install` a binary crate from git or path.
#[cfg(feature = "cargo")]
pub mod cargo_install;

/// `DockerBuild` action — `docker build` an image from the node's checkout.
#[cfg(feature = "docker")]
pub mod docker;
