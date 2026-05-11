//! Concrete [`repolith_core::action::Action`] implementations.
//!
//! Each backend is gated behind its own cargo feature so consumers only pull
//! in what they need:
//!
//! - `git`: [`git_clone::GitClone`] — mirror a remote git repo.
//! - `cargo`: [`cargo_install::CargoInstall`] — `cargo install --git/--path`.

#[cfg(any(feature = "git", feature = "cargo"))]
pub(crate) mod util;

/// `GitClone` action — fetch a remote git repository into a local path.
#[cfg(feature = "git")]
pub mod git_clone;

/// `CargoInstall` action — `cargo install` a binary crate from git or path.
#[cfg(feature = "cargo")]
pub mod cargo_install;
