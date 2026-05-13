//! Path utilities shared by every action and by the manifest factory.
//!
//! Currently exposes a single helper, `expand_tilde`, because the rest
//! of the codebase relies on `std::path::PathBuf` directly.

use std::path::{Path, PathBuf};

/// Expand a leading `~/` or bare `~` in `p` to [`dirs::home_dir`].
///
/// Other paths (absolute without `~`, relative, anything starting with
/// `~user/`) are returned unchanged.
///
/// Neither `cargo` nor `git` performs this expansion themselves, so a
/// manifest that writes `install_to = "~/.local/bin"` or
/// `path = "~/code/foo"` would otherwise create a literal `~` directory
/// next to the orchestrator's cwd. Apply this helper before passing
/// user-supplied paths to subprocesses.
#[must_use]
pub fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if s == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    p.to_path_buf()
}
