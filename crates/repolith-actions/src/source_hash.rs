//! Shared input-hash ingredients: build platform and source-tree digest.
//!
//! Both live here rather than in each action because getting either one
//! wrong produces the same failure — a cache that reports `up to date`
//! while the artifact is stale — and that failure is silent.
//!
//! # Why the platform belongs in the hash
//!
//! `cargo --version` carries no target triple (`cargo -vV` does), so
//! without `platform_tag` an aarch64 macOS build and an x86_64 Linux
//! build of the same source hash identically. Harmless with a per-machine
//! `SQLite` cache; wrong the moment a Neo4j backend is shared, where the
//! second machine reads the first's entry and installs nothing at all.
//!
//! `RUSTFLAGS` is deliberately absent: it is not in the CLI's
//! `ENV_ALLOWLIST`, so it never reaches cargo and cannot change the
//! build. `RUSTUP_TOOLCHAIN` *is* allowlisted, but is covered
//! transitively — `cargo --version` runs through the rustup shim and
//! reports the resolved toolchain.
//!
//! # Why the digest is content-only
//!
//! No mtimes: a fresh CI clone rewrites them all and would make every
//! action look stale forever. Hashing content instead keeps the digest
//! stable across machines, which a shared cache backend requires.
//!
//! The walk honors `.gitignore` and always skips `.git/` and `target/` —
//! hashing build output would be both enormous and self-defeating, since
//! it changes on every build and would trigger a rebuild on every sync.

use ignore::WalkBuilder;
use repolith_core::types::{BuildError, Sha256};
use sha2::{Digest, Sha256 as ShaHasher};
use std::path::Path;

/// Feed the build platform into a hasher.
///
/// Compile-time constants of the running `repolith` binary — no
/// subprocess. repolith has no cross-compilation support, so the host
/// platform *is* the target platform.
pub fn platform_tag(h: &mut ShaHasher) {
    h.update(b":arch:");
    h.update(std::env::consts::ARCH.as_bytes());
    h.update(b":os:");
    h.update(std::env::consts::OS.as_bytes());
}

/// Content digest of the source tree rooted at `path`.
///
/// Hashes each file's path-relative-to-root and its bytes, in a stable
/// (sorted) order so two machines agree. Honors `.gitignore`; always
/// skips `.git/` and `target/`.
///
/// # Errors
/// [`BuildError::Io`] when the root cannot be walked. Individual
/// unreadable entries are folded into the digest as a marker rather than
/// aborting: a permission-denied file is a real input state, and failing
/// the whole plan over one unreadable path would be worse than rebuilding.
pub fn tree_digest(path: &Path) -> Result<Sha256, BuildError> {
    if !path.exists() {
        return Err(BuildError::Io(format!(
            "source tree {} does not exist",
            path.display()
        )));
    }

    // Collect first, then sort: `ignore`'s walk order is filesystem
    // dependent, and an unstable order would produce a different digest
    // for identical content.
    let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
    let walker = WalkBuilder::new(path)
        .hidden(false) // .config/, .cargo/ etc. are real inputs
        // The digest must depend on the tree's own content and nothing
        // else, or two machines will disagree. So: honor ignore files
        // found *inside* the tree, and no others.
        .git_ignore(true)
        // Apply the tree's own `.gitignore` whether or not it is a
        // worktree — otherwise a git clone and a vendored copy of the
        // same sources would digest differently.
        .require_git(false)
        // Never walk up. A stray `.gitignore` in a parent (a real one
        // lives in macOS `$TMPDIR`) would otherwise silently swallow the
        // whole tree, and its presence differs from machine to machine.
        .parents(false)
        .git_global(false) // machine-local global ignore
        .git_exclude(false) // `.git/info/exclude` is per-clone, not committed
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some(".git" | "target" | ".repolith")
            )
        })
        .build();

    for entry in walker {
        let entry = entry.map_err(|e| BuildError::Io(format!("walk {}: {e}", path.display())))?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(path)
            .unwrap_or(entry.path())
            .to_string_lossy()
            // Normalize separators so a Windows peer and a Unix peer
            // agree on the digest of the same tree.
            .replace('\\', "/");
        entries.push((rel, entry.path().to_path_buf()));
    }
    entries.sort();

    let mut h = ShaHasher::new();
    h.update(b"tree:v1:");
    for (rel, abs) in &entries {
        h.update(rel.as_bytes());
        h.update(b"\0");
        match std::fs::read(abs) {
            Ok(bytes) => {
                h.update(b"ok:");
                h.update(&bytes);
            }
            // Unreadable is itself a state worth distinguishing from
            // absent or empty; don't fail the plan over it.
            Err(e) => {
                h.update(b"unreadable:");
                h.update(e.kind().to_string().as_bytes());
            }
        }
        h.update(b"\n");
    }
    Ok(Sha256(h.finalize().into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn digest(dir: &Path) -> Sha256 {
        tree_digest(dir).expect("digest")
    }

    #[test]
    fn missing_tree_is_an_error() {
        assert!(tree_digest(Path::new("/nonexistent/repolith/tree")).is_err());
    }

    #[test]
    fn same_content_same_digest() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for d in [a.path(), b.path()] {
            fs::create_dir(d.join("src")).unwrap();
            fs::write(d.join("src/main.rs"), b"fn main() {}").unwrap();
            fs::write(d.join("Cargo.toml"), b"[package]\nname=\"x\"").unwrap();
        }
        assert_eq!(digest(a.path()), digest(b.path()), "content-only digest");
    }

    #[test]
    fn one_byte_edit_changes_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.rs"), b"fn main() {}").unwrap();
        let before = digest(dir.path());
        fs::write(dir.path().join("f.rs"), b"fn main() { }").unwrap();
        assert_ne!(before, digest(dir.path()), "edit must be visible");
    }

    #[test]
    fn touch_alone_does_not_change_the_digest() {
        // The regression that mtime-based hashing would introduce: a
        // fresh clone rewrites every mtime and would look fully stale.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.rs");
        fs::write(&f, b"same").unwrap();
        let before = digest(dir.path());
        let content = fs::read(&f).unwrap();
        fs::write(&f, &content).unwrap(); // rewrite → new mtime, same bytes
        assert_eq!(before, digest(dir.path()), "mtime must not leak in");
    }

    #[test]
    fn target_and_git_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.rs"), b"src").unwrap();
        let before = digest(dir.path());

        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/huge.bin"), vec![0u8; 4096]).unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/HEAD"), b"ref: refs/heads/main").unwrap();

        assert_eq!(
            before,
            digest(dir.path()),
            "build output and git internals must not be hashed"
        );
    }

    #[test]
    fn gitignored_files_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), b"secret.txt\n").unwrap();
        fs::write(dir.path().join("f.rs"), b"src").unwrap();
        let before = digest(dir.path());
        fs::write(dir.path().join("secret.txt"), b"ignored").unwrap();
        assert_eq!(before, digest(dir.path()), ".gitignore must be honored");
    }

    #[test]
    fn adding_a_file_changes_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), b"a").unwrap();
        let before = digest(dir.path());
        fs::write(dir.path().join("b.rs"), b"b").unwrap();
        assert_ne!(before, digest(dir.path()), "new file must be visible");
    }

    #[test]
    fn renaming_changes_the_digest() {
        // Path is hashed alongside content, so a pure rename is a change.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), b"same").unwrap();
        let before = digest(dir.path());
        fs::rename(dir.path().join("a.rs"), dir.path().join("b.rs")).unwrap();
        assert_ne!(before, digest(dir.path()));
    }

    #[test]
    fn platform_tag_is_mixed_in() {
        let mut with = ShaHasher::new();
        with.update(b"x");
        platform_tag(&mut with);
        let mut without = ShaHasher::new();
        without.update(b"x");
        assert_ne!(
            with.finalize().to_vec(),
            without.finalize().to_vec(),
            "platform must contribute to the hash"
        );
    }
}
