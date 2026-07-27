//! The **local source closure** of a cargo package: every tree on this
//! machine whose contents can change the binary `cargo install` produces.
//!
//! # Why a single tree is not enough
//!
//! [`crate::source_hash::tree_digest`] hashes one directory. For a workspace
//! member that is not the whole input: editing a crate reached through a
//! `path` dependency changes the binary while leaving the digest untouched,
//! so `sync` reports `up to date` and the installed binary silently keeps
//! stale code (issue #78). Same failure class as #73 one level up — #73 made
//! the digest see the tree, this makes it see everything the tree is built
//! from.
//!
//! # Why `cargo metadata` and not a `Cargo.toml` parser
//!
//! Following `dependencies.*.path` by hand means reimplementing workspace
//! inheritance (`workspace = true`), `[workspace.dependencies]`,
//! `[target.'cfg(…)'.dependencies]` and `[patch]`. Getting any of those
//! subtly wrong reintroduces exactly the silent under-invalidation this
//! module exists to remove. `cargo metadata --no-deps` is cargo's own
//! resolution of all of it, costs ~10 ms, needs no network and no lockfile,
//! and reports each dependency's absolute path and kind.
//!
//! # What goes into the digest
//!
//! - the package's own tree;
//! - the tree of every `path` dependency reachable from it, transitively,
//!   deduplicated;
//! - each encountered workspace root's `Cargo.toml` and `Cargo.lock`.
//!
//! That last one closes a hole a path-dependency fix alone would leave open:
//! `[workspace.dependencies]`, `[patch]` and `[profile.release]` live in the
//! workspace root manifest, *outside* the member's directory, and all three
//! change the produced binary.
//!
//! **Dev-dependencies are excluded.** `cargo install` builds the binary
//! target and its normal + build dependencies; dev-dependencies serve tests,
//! benches and examples and are never compiled. Including them would be
//! free over-invalidation.
//!
//! # Cross-machine stability
//!
//! Absolute paths never enter the digest — two machines checking the same
//! sources out to different directories must agree, or a shared Neo4j cache
//! backend is worse than useless. Trees therefore contribute only their
//! content digests, and those digests are **sorted by value**, not by path,
//! so the ordering is content-determined rather than layout-determined.

use crate::source_hash::tree_digest;
use crate::util::run_with_cancel;
use repolith_core::types::{BuildError, Sha256};
use sha2::{Digest, Sha256 as ShaHasher};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Content digest of `root` and of every local tree it is built from.
///
/// Falls back to `root` alone when the closure cannot be resolved — cargo
/// missing, or a directory that is not a cargo package. That is exactly the
/// pre-#78 behaviour, so nothing regresses; the digest carries a distinct
/// marker so a resolved package and a fallback package can never collide on
/// the same hash and steal each other's cache entry.
///
/// # Errors
/// [`BuildError::Io`] when `root` itself cannot be walked. An individual
/// *dependency* tree that has gone missing is folded in as a marker instead:
/// it is a real input state, and failing the whole plan over one absent path
/// would be worse than rebuilding.
pub async fn source_closure_digest(
    root: &Path,
    cancel: &CancellationToken,
) -> Result<Sha256, BuildError> {
    let mut h = ShaHasher::new();
    h.update(b"closure:v1:");

    let Some(closure) = local_closure(root, cancel).await else {
        h.update(b"unresolved:");
        h.update(tree_digest(root)?.0);
        return Ok(Sha256(h.finalize().into()));
    };

    h.update(b"resolved:root:");
    h.update(tree_digest(root)?.0);

    // Sorted by digest value, never by path — see the module docs.
    let mut ingredients: BTreeSet<[u8; 32]> = BTreeSet::new();
    for tree in &closure.dep_trees {
        ingredients.insert(tree_digest_or_marker(tree).0);
    }
    for ws in &closure.workspace_roots {
        ingredients.insert(file_digest(b"ws-manifest:", &ws.join("Cargo.toml")).0);
        ingredients.insert(file_digest(b"ws-lock:", &ws.join("Cargo.lock")).0);
    }
    for d in &ingredients {
        h.update(b":in:");
        h.update(d);
    }
    Ok(Sha256(h.finalize().into()))
}

/// A dependency tree that has vanished is a state worth distinguishing from
/// an empty one, and worth rebuilding over — but not worth failing the whole
/// plan for.
fn tree_digest_or_marker(path: &Path) -> Sha256 {
    tree_digest(path).unwrap_or_else(|_| {
        let mut h = ShaHasher::new();
        h.update(b"absent-dep-tree");
        Sha256(h.finalize().into())
    })
}

/// Content digest of a single file, `label`-tagged so two different files
/// with identical bytes (an empty `Cargo.toml` and an empty `Cargo.lock`)
/// stay distinguishable.
fn file_digest(label: &[u8], path: &Path) -> Sha256 {
    let mut h = ShaHasher::new();
    h.update(label);
    match std::fs::read(path) {
        Ok(bytes) => {
            h.update(b"ok:");
            h.update(&bytes);
        }
        // Absent is normal: a package outside any workspace has no root
        // lockfile, and `Cargo.lock` is often gitignored for libraries.
        Err(_) => h.update(b"absent"),
    }
    Sha256(h.finalize().into())
}

/// Every local tree reachable from `root`, and every workspace root seen.
struct Closure {
    /// Path-dependency trees, excluding `root` itself.
    dep_trees: BTreeSet<PathBuf>,
    /// Workspace roots whose manifest and lockfile are inputs.
    workspace_roots: BTreeSet<PathBuf>,
}

/// Walk the local dependency graph outward from `root`.
///
/// Returns `None` only when `root` itself cannot be resolved — that is the
/// signal to fall back to a single-tree digest. A *dependency* that cannot
/// be resolved still contributes its own tree; only its transitive deps are
/// lost, which is a narrower gap than failing outright.
///
/// Termination is by construction: every directory is canonicalized and
/// visited at most once, so symlink loops and dependency cycles both stop.
async fn local_closure(root: &Path, cancel: &CancellationToken) -> Option<Closure> {
    let root = root.canonicalize().ok()?;

    // package directory -> its path dependencies. One `cargo metadata` call
    // populates every member of a workspace at once, so a workspace is
    // scanned once no matter how many of its members are reachable.
    let mut graph: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    let mut workspace_roots: BTreeSet<PathBuf> = BTreeSet::new();
    let mut dep_trees: BTreeSet<PathBuf> = BTreeSet::new();
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue: Vec<PathBuf> = vec![root.clone()];

    while let Some(dir) = queue.pop() {
        if !visited.insert(dir.clone()) {
            continue;
        }
        if !graph.contains_key(&dir) {
            match run_metadata(&dir, cancel).await {
                Some(meta) => {
                    workspace_roots.insert(meta.workspace_root);
                    for (pkg_dir, deps) in meta.packages {
                        graph.entry(pkg_dir).or_insert(deps);
                    }
                }
                // The root not resolving means we know nothing: fall back.
                None if dir == root => return None,
                // A dependency that will not resolve still had its tree
                // recorded by whoever pointed at it.
                None => continue,
            }
        }
        if let Some(deps) = graph.get(&dir).cloned() {
            for dep in deps {
                let Ok(dep) = dep.canonicalize() else {
                    continue;
                };
                if dep != root {
                    dep_trees.insert(dep.clone());
                }
                queue.push(dep);
            }
        }
    }

    Some(Closure {
        dep_trees,
        workspace_roots,
    })
}

/// What one `cargo metadata --no-deps` invocation tells us.
struct Meta {
    workspace_root: PathBuf,
    /// `(package directory, its path dependencies)` for every workspace member.
    packages: Vec<(PathBuf, Vec<PathBuf>)>,
}

async fn run_metadata(dir: &Path, cancel: &CancellationToken) -> Option<Meta> {
    let mut cmd = Command::new("cargo");
    cmd.arg("metadata")
        .arg("--no-deps")
        .arg("--format-version")
        .arg("1")
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"));
    // Cancellation-aware: `--no-deps` needs no network in the ordinary case,
    // but a `[patch]` pointing at a git source can still make cargo fetch,
    // and a plan must stay interruptible.
    let out = run_with_cancel(cmd, cancel).await.ok()?;
    if !out.status.success() {
        return None;
    }
    parse_metadata(&out.stdout)
}

/// Pull the workspace root and each member's path dependencies out of
/// `cargo metadata` output.
///
/// Navigated as untyped JSON rather than through derived structs: three
/// fields are needed out of a document with dozens, and a strict struct
/// would fail to deserialize the day cargo adds a field.
fn parse_metadata(bytes: &[u8]) -> Option<Meta> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let workspace_root = PathBuf::from(v.get("workspace_root")?.as_str()?);

    let mut packages = Vec::new();
    for p in v.get("packages")?.as_array()? {
        let Some(manifest) = p.get("manifest_path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(dir) = Path::new(manifest).parent() else {
            continue;
        };
        let mut deps = Vec::new();
        for d in p
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            // `kind` is null for a normal dependency, "dev" or "build"
            // otherwise. Only dev-dependencies are never compiled.
            if d.get("kind").and_then(serde_json::Value::as_str) == Some("dev") {
                continue;
            }
            if let Some(path) = d.get("path").and_then(serde_json::Value::as_str) {
                deps.push(PathBuf::from(path));
            }
        }
        packages.push((dir.to_path_buf(), deps));
    }
    Some(Meta {
        workspace_root,
        packages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    /// A two-crate workspace where `app` depends on `lib` by path.
    fn workspace(dir: &Path) {
        fs::write(
            dir.join("Cargo.toml"),
            b"[workspace]\nmembers = [\"app\", \"lib\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        for (name, extra) in [
            ("app", "\n[dependencies]\nlib = { path = \"../lib\" }\n"),
            ("lib", ""),
        ] {
            let c = dir.join(name);
            fs::create_dir_all(c.join("src")).unwrap();
            fs::write(
                c.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}"
                ),
            )
            .unwrap();
            fs::write(c.join("src/lib.rs"), b"").unwrap();
        }
        fs::write(dir.join("app/src/main.rs"), b"fn main() {}").unwrap();
    }

    /// The bug this module exists for: editing a path dependency must move
    /// the dependent's digest.
    #[tokio::test]
    async fn editing_a_path_dependency_changes_the_digest() {
        let tmp = tempfile::tempdir().unwrap();
        workspace(tmp.path());
        let app = tmp.path().join("app");

        let before = source_closure_digest(&app, &cancel()).await.unwrap();
        fs::write(tmp.path().join("lib/src/lib.rs"), b"pub fn added() {}").unwrap();
        let after = source_closure_digest(&app, &cancel()).await.unwrap();

        assert_ne!(
            before, after,
            "a path dependency's content is an input — this is issue #78"
        );
    }

    /// The other half: not everything nearby is an input.
    #[tokio::test]
    async fn editing_an_unrelated_file_does_not_change_the_digest() {
        let tmp = tempfile::tempdir().unwrap();
        workspace(tmp.path());
        let app = tmp.path().join("app");

        let before = source_closure_digest(&app, &cancel()).await.unwrap();
        fs::write(tmp.path().join("README.md"), b"# docs, not code\n").unwrap();
        let after = source_closure_digest(&app, &cancel()).await.unwrap();

        assert_eq!(
            before, after,
            "a file in the workspace root that is not an input must not \
             trigger a rebuild — over-invalidation is a cost, not a free win"
        );
    }

    /// `[workspace.dependencies]` / `[patch]` / `[profile]` live here and
    /// change the produced binary, yet sit outside the member's directory.
    #[tokio::test]
    async fn editing_the_workspace_manifest_changes_the_digest() {
        let tmp = tempfile::tempdir().unwrap();
        workspace(tmp.path());
        let app = tmp.path().join("app");

        let before = source_closure_digest(&app, &cancel()).await.unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            b"[workspace]\nmembers = [\"app\", \"lib\"]\nresolver = \"2\"\n\
              \n[profile.release]\nopt-level = \"z\"\n",
        )
        .unwrap();
        let after = source_closure_digest(&app, &cancel()).await.unwrap();

        assert_ne!(
            before, after,
            "the workspace root manifest is an input to every member"
        );
    }

    /// A dependency's dependency counts too.
    #[tokio::test]
    async fn the_closure_is_transitive() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("Cargo.toml"),
            b"[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        for (name, dep) in [("a", Some("b")), ("b", Some("c")), ("c", None)] {
            let d = root.join(name);
            fs::create_dir_all(d.join("src")).unwrap();
            let extra = dep.map_or_else(String::new, |x| {
                format!("\n[dependencies]\n{x} = {{ path = \"../{x}\" }}\n")
            });
            fs::write(
                d.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}"
                ),
            )
            .unwrap();
            fs::write(d.join("src/lib.rs"), b"").unwrap();
        }

        let a = root.join("a");
        let before = source_closure_digest(&a, &cancel()).await.unwrap();
        fs::write(root.join("c/src/lib.rs"), b"pub fn deep() {}").unwrap();
        let after = source_closure_digest(&a, &cancel()).await.unwrap();

        assert_ne!(before, after, "a -> b -> c: editing c must reach a");
    }

    /// Dev-dependencies are not compiled by `cargo install`, so a path
    /// dev-dependency must not drag its whole tree into the digest.
    #[tokio::test]
    async fn a_path_dev_dependency_is_not_an_input() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("Cargo.toml"),
            b"[workspace]\nmembers = [\"app\", \"harness\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        for (name, extra) in [
            (
                "app",
                "\n[dev-dependencies]\nharness = { path = \"../harness\" }\n",
            ),
            ("harness", ""),
        ] {
            let d = root.join(name);
            fs::create_dir_all(d.join("src")).unwrap();
            fs::write(
                d.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}"
                ),
            )
            .unwrap();
            fs::write(d.join("src/lib.rs"), b"").unwrap();
        }

        let app = root.join("app");
        let before = source_closure_digest(&app, &cancel()).await.unwrap();
        fs::write(root.join("harness/src/lib.rs"), b"pub fn helper() {}").unwrap();
        let after = source_closure_digest(&app, &cancel()).await.unwrap();

        assert_eq!(
            before, after,
            "dev-dependencies are never compiled by `cargo install`"
        );
    }

    /// A directory that is not a cargo package must still produce a digest,
    /// and must not be confused with a resolved one.
    #[tokio::test]
    async fn a_non_cargo_directory_falls_back_without_colliding() {
        let plain = tempfile::tempdir().unwrap();
        fs::write(plain.path().join("notes.txt"), b"no Cargo.toml here").unwrap();

        let fallback = source_closure_digest(plain.path(), &cancel())
            .await
            .expect("fallback must still produce a digest");
        assert_eq!(
            fallback,
            source_closure_digest(plain.path(), &cancel())
                .await
                .unwrap(),
            "the fallback must be deterministic"
        );

        // The marker must keep the two shapes apart: a bare tree_digest of
        // the same directory must not equal the fallback closure digest,
        // or a resolved and an unresolved package could share a cache entry.
        assert_ne!(
            fallback.0,
            tree_digest(plain.path()).unwrap().0,
            "resolved and unresolved digests must never collide"
        );
    }

    #[tokio::test]
    async fn a_missing_root_is_an_error_not_a_silent_digest() {
        assert!(
            source_closure_digest(Path::new("/nonexistent/repolith/pkg"), &cancel())
                .await
                .is_err()
        );
    }

    #[test]
    fn parse_metadata_keeps_build_deps_and_drops_dev_deps() {
        let json = br#"{
          "workspace_root": "/ws",
          "packages": [{
            "manifest_path": "/ws/app/Cargo.toml",
            "dependencies": [
              {"name": "normal",   "kind": null,    "path": "/ws/normal"},
              {"name": "builder",  "kind": "build", "path": "/ws/builder"},
              {"name": "harness",  "kind": "dev",   "path": "/ws/harness"},
              {"name": "registry", "kind": null,    "path": null}
            ]
          }]
        }"#;
        let meta = parse_metadata(json).expect("parses");
        assert_eq!(meta.workspace_root, PathBuf::from("/ws"));
        let (dir, deps) = &meta.packages[0];
        assert_eq!(dir, &PathBuf::from("/ws/app"));
        assert_eq!(
            deps,
            &vec![PathBuf::from("/ws/normal"), PathBuf::from("/ws/builder")],
            "normal and build deps are compiled; dev deps are not; \
             registry deps have no local tree"
        );
    }

    #[test]
    fn parse_metadata_survives_unknown_fields() {
        // A strict struct would break the day cargo adds a field.
        let json = br#"{
          "workspace_root": "/ws",
          "some_future_field": {"a": 1},
          "packages": [{
            "manifest_path": "/ws/app/Cargo.toml",
            "brand_new_key": 42,
            "dependencies": []
          }]
        }"#;
        assert!(parse_metadata(json).is_some());
    }
}
