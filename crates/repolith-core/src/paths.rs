//! Run-time path containment — the filesystem half of the two-stage
//! validation used by actions that resolve user-supplied relative paths
//! (`docker`'s `dockerfile`/`context`, federation's `manifest`).
//!
//! The lexical half lives in the manifest validator
//! (`check_contained_path`): relative-only, no `..` component, no leading
//! dash, no control characters. That check cannot see symlinks — `sub`
//! pointing at `/etc` is lexically clean. `contain_within` closes the
//! hole by canonicalizing and requiring the result to stay inside the
//! canonicalized base.

use crate::types::BuildError;
use std::path::{Path, PathBuf};

/// Resolve `relative` against `base` and require the canonicalized result
/// to stay inside canonicalized `base`.
///
/// # Errors
/// - [`BuildError::Io`] when either canonicalization fails (missing file,
///   permission) or when the resolved path escapes `base` (symlink
///   traversal).
pub fn contain_within(base: &Path, relative: &Path) -> Result<PathBuf, BuildError> {
    let base_canon = base
        .canonicalize()
        .map_err(|e| BuildError::Io(format!("canonicalize base {}: {e}", base.display())))?;
    let joined = base_canon.join(relative);
    let canon = joined
        .canonicalize()
        .map_err(|e| BuildError::Io(format!("canonicalize {}: {e}", joined.display())))?;
    if !canon.starts_with(&base_canon) {
        return Err(BuildError::Io(format!(
            "path {} escapes the node checkout {} (symlink traversal?)",
            canon.display(),
            base_canon.display()
        )));
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_inner_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let got = contain_within(dir.path(), Path::new("sub")).unwrap();
        assert!(got.ends_with("sub"));
    }

    #[test]
    fn rejects_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(contain_within(dir.path(), Path::new("absent")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("evil")).unwrap();
        let err = contain_within(dir.path(), Path::new("evil")).unwrap_err();
        assert!(err.to_string().contains("escapes"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_file_escape() {
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, b"x").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&secret, dir.path().join("Dockerfile")).unwrap();
        let err = contain_within(dir.path(), Path::new("Dockerfile")).unwrap_err();
        assert!(err.to_string().contains("escapes"), "got: {err}");
    }
}
