//! Shared helpers for end-to-end smoke tests.
//!
//! Each test gets a fresh tempdir whose layout looks like:
//! ```text
//! <workdir>/
//!   fixture/                # copy of tests/fixtures/smoke/
//!     node-a/.git/          # created by setup via `git init` + commit
//!     node-a/...
//!     node-bc-lib/...
//!   repolith.toml           # rendered from .template with absolute paths
//!   cache.db                # SqliteCache file (created by the binary)
//!   clones/                 # GitClone destination
//!   bin/                    # CargoInstall --root destination
//! ```

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

pub struct SmokeEnv {
    /// Lifetime guard for the tempdir.
    pub workdir: TempDir,
    /// Absolute path to the per-test fixture copy.
    pub fixture: PathBuf,
    /// Absolute path of the rendered manifest.
    pub manifest: PathBuf,
    /// Absolute path of the cache database.
    pub cache: PathBuf,
}

impl SmokeEnv {
    pub fn workdir_path(&self) -> &Path {
        self.workdir.path()
    }
}

/// Run `git` with `args` inside `cwd`; panics on failure (helpful in tests).
fn run_git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("git binary must be on PATH");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

fn fixture_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("smoke")
}

fn render_template(template_path: &Path, fixture: &Path, workdir: &Path, out: &Path) {
    let raw = std::fs::read_to_string(template_path)
        .unwrap_or_else(|_| panic!("read template {}", template_path.display()));
    let rendered = raw
        .replace("${SMOKE_FIXTURE}", &fixture.display().to_string())
        .replace("${WORKDIR}", &workdir.display().to_string());
    std::fs::write(out, rendered).unwrap();
}

fn git_init_node_a(node_a: &Path) {
    run_git(node_a, &["init", "--initial-branch=main", "--quiet"]);
    run_git(node_a, &["config", "user.email", "smoke@example.com"]);
    run_git(node_a, &["config", "user.name", "Smoke"]);
    run_git(node_a, &["config", "commit.gpgsign", "false"]);
    run_git(node_a, &["add", "."]);
    run_git(node_a, &["commit", "-m", "initial", "--quiet"]);
    // Make HEAD a symbolic ref to main so `git ls-remote HEAD` resolves cleanly.
    run_git(node_a, &["symbolic-ref", "HEAD", "refs/heads/main"]);
}

/// Build a fresh smoke env: copy fixture, render the main manifest, init git
/// on node-a, return the env with absolute paths plumbed through.
pub fn setup_smoke_fixture() -> SmokeEnv {
    setup_with_template("repolith.toml.template")
}

/// Same as [`setup_smoke_fixture`] but uses the failing manifest variant
/// (single node, action[0] = unreachable git-clone).
pub fn setup_smoke_fixture_failfast() -> SmokeEnv {
    setup_with_template("repolith-failfast.toml.template")
}

fn setup_with_template(template_name: &str) -> SmokeEnv {
    let workdir = TempDir::new().expect("tempdir for smoke env");
    let fixture = workdir.path().join("fixture");
    copy_dir_recursive(&fixture_src(), &fixture);

    // Only the standard fixture needs node-a to be a real git repo.
    let node_a = fixture.join("node-a");
    if node_a.is_dir() {
        git_init_node_a(&node_a);
    }

    let manifest = workdir.path().join("repolith.toml");
    render_template(
        &fixture_src().join(template_name),
        &fixture,
        workdir.path(),
        &manifest,
    );

    let cache = workdir.path().join("cache.db");

    SmokeEnv {
        workdir,
        fixture,
        manifest,
        cache,
    }
}

/// Spawn the `repolith` binary with `--manifest` and `--cache-path` pre-wired
/// to this env, plus the supplied `args` appended.
pub fn run_repolith(env: &SmokeEnv, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_repolith");
    Command::new(bin)
        .arg("--manifest")
        .arg(&env.manifest)
        .arg("--cache-path")
        .arg(&env.cache)
        .args(args)
        .env_remove("RUST_LOG")
        .output()
        .expect("spawn repolith")
}

/// Append a new commit to the node-a fixture so its HEAD sha changes.
pub fn add_commit_to_node_a(env: &SmokeEnv, msg: &str) {
    let node_a = env.fixture.join("node-a");
    std::fs::write(node_a.join("CHANGES.md"), format!("{msg}\n")).unwrap();
    run_git(&node_a, &["add", "."]);
    run_git(&node_a, &["commit", "-m", msg, "--quiet"]);
}
