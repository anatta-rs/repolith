//! End-to-end tests for the `repolith` CLI via `assert_cmd`.
//!
//! Each test invokes the binary as a subprocess against a fixture manifest.
//! `--dry-run` flows never write to the cache file — but we still point
//! `--cache-path` at a tempdir to keep tests fully isolated.

use assert_cmd::Command;
use predicates::boolean::PredicateBooleanExt;
use predicates::str;
use std::path::PathBuf;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn cmd() -> Command {
    let mut c = Command::cargo_bin("repolith").expect("repolith binary must be built");
    // Stop the test from inheriting RUST_LOG noise from the dev shell.
    c.env_remove("RUST_LOG");
    c
}

#[test]
fn version_flag_prints_semver() {
    cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(str::contains("repolith"));
}

#[test]
fn help_lists_all_subcommands() {
    cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(str::contains("sync"))
        .stdout(str::contains("status"));
}

#[test]
fn dry_run_empty_manifest_reports_zero() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("empty.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("sync")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(str::contains("0 action(s) would run"));
}

#[test]
fn dry_run_minimal_manifest_reports_one() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("minimal.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("sync")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(str::contains("1 action(s) would run"));
}

#[test]
fn explain_prints_change_reason() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("minimal.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("sync")
        .arg("--explain")
        .arg("--dry-run")
        .assert()
        .success()
        // Human-readable rendering, not the Debug dump that used to blow
        // the table to ~400 columns (issue #86).
        .stdout(str::contains("never built"));
}

#[test]
fn invalid_schema_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("invalid_schema.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("sync")
        .arg("--dry-run")
        .assert()
        .failure()
        .stderr(str::contains("schema"));
}

#[test]
fn status_prints_table_with_action_row() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("minimal.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("status")
        .assert()
        .success()
        .stdout(str::contains("local-thing::cargo-install::0"))
        .stdout(str::contains("stale"));
}

/// The no-argument path must keep rendering a table, not a detail block —
/// adding the filter must not quietly change what `status` alone does.
#[test]
fn status_without_filter_still_renders_a_table() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("two_nodes.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("status")
        .assert()
        .success()
        // Table furniture, present only in the tabular rendering.
        .stdout(str::contains("Action"))
        .stdout(str::contains("Reason"))
        .stdout(str::contains("+---"))
        .stdout(str::contains("state       ").not());
}

#[test]
fn status_with_filter_prints_a_detail_block() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("two_nodes.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("status")
        .arg("beta")
        .assert()
        .success()
        .stdout(str::contains("beta::cargo-install::0"))
        .stdout(str::contains("state"))
        .stdout(str::contains("last run"))
        .stdout(str::contains("action"))
        // The declared inputs the table has no column for.
        .stdout(str::contains("profile"))
        // …and only the matching node.
        .stdout(str::contains("alpha").not());
}

/// A filter naming a node must expand every action of that node, and the
/// resolved TOML keys must reach the output.
#[test]
fn status_filter_matches_every_action_of_a_node() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("two_nodes.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("status")
        .arg("alpha")
        .assert()
        .success()
        .stdout(str::contains("alpha::cargo-install::0"))
        // Declared inputs, straight from the fixture — the table has no
        // column for any of these.
        .stdout(str::contains("alpha-pkg"))
        .stdout(str::contains("dev"))
        .stdout(str::contains("extra"))
        .stdout(str::contains("deps"));
}

/// A filter matching nothing is a failed query, not an empty success —
/// otherwise a typo is indistinguishable from a healthy action.
#[test]
fn status_filter_matching_nothing_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("two_nodes.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("status")
        .arg("gamma")
        .assert()
        .failure()
        .stderr(str::contains("gamma"));
}

/// Create a git repository with one commit, and return its path. Cloning it
/// over `file://` needs no network, so `git-clone::input_hash` — which runs
/// `git ls-remote` unconditionally — works offline.
fn local_repo(at: &std::path::Path) -> PathBuf {
    let repo = at.join("origin");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            // Ignore the developer's own git config. Without this the
            // commit below inherits `commit.gpgsign=true` and blocks on a
            // signing agent that CI does not have — a 60-second hang, not
            // a clean failure.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git must be on PATH");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "--quiet", "--initial-branch=main"]);
    // The repo deliberately contains no Cargo.toml: the clone succeeds and
    // the cargo-install that depends on it fails immediately, which is the
    // pair of outcomes this test needs — and it needs no compilation.
    std::fs::write(repo.join("README.md"), "not a crate\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.email=t@example.com",
        "-c",
        "user.name=t",
        "commit",
        "--quiet",
        "-m",
        "init",
    ]);
    repo
}

/// The reason this view exists: `sync` records a failure, and the detail
/// block hands back the whole error the table had to cut at 72 characters.
///
/// Also the one place a real dependency edge is exercised — `cargo-install`
/// after `git-clone` within a node.
#[test]
fn status_detail_shows_the_full_error_and_the_dependency_edge() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache.db");
    let manifest = tmp.path().join("failing.toml");
    let origin = local_repo(tmp.path());

    // Written at test time: both the remote URL and the checkout path are
    // test-local, which a checked-in fixture cannot know.
    std::fs::write(
        &manifest,
        format!(
            r#"
[orchestrator]
schema_version = "0.1"
name = "failing"

[[node]]
id = "nope"
git = "file://{}"
path = "{}"

  [[node.action]]
  kind = "git-clone"

  [[node.action]]
  kind = "cargo-install"
  crate = "nope"
"#,
            origin.display(),
            tmp.path().join("checkout").display()
        ),
    )
    .unwrap();

    cmd()
        .arg("--manifest")
        .arg(&manifest)
        .arg("--cache-path")
        .arg(&cache)
        .arg("sync")
        .assert()
        .failure();

    cmd()
        .arg("--manifest")
        .arg(&manifest)
        .arg("--cache-path")
        .arg(&cache)
        .arg("status")
        .arg("nope::cargo-install")
        .assert()
        .success()
        .stdout(str::contains("nope::cargo-install::1"))
        // The cached failure is named as such, not as "never built".
        .stdout(str::contains("previous run failed"))
        .stdout(str::contains("last run"))
        // The dependency edge, with the dep's own state.
        .stdout(str::contains("nope::git-clone::0"))
        // The timestamp exists now (#88), so the age is a real answer.
        .stdout(str::contains("date not recorded").not());
}

#[test]
fn status_help_documents_the_filter() {
    cmd()
        .arg("status")
        .arg("--help")
        .assert()
        .success()
        .stdout(str::contains("FILTER"));
}

// ---------------------------------------------------------------------------
// `sync --force [FILTER]` — issue #95.
// ---------------------------------------------------------------------------

#[test]
fn force_marks_a_fresh_manifest_forced_not_never_built() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("two_nodes.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("sync")
        .arg("--dry-run")
        .arg("--explain")
        .arg("--force")
        .assert()
        .success()
        .stdout(str::contains("forced"))
        .stdout(str::contains("2 action(s) would run"));
}

#[test]
fn force_with_a_filter_narrows_to_matching_actions() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("two_nodes.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("sync")
        .arg("--dry-run")
        .arg("--explain")
        .arg("--force")
        .arg("beta")
        .assert()
        .success()
        .stdout(str::contains("beta::cargo-install::0: forced"))
        // `alpha` is still stale (never built) but must NOT be labelled forced.
        .stdout(str::contains("alpha::cargo-install::0: forced").not());
}

/// A filter matching nothing is a failed request, mirroring `status <filter>`.
#[test]
fn force_with_an_unmatched_filter_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("two_nodes.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("sync")
        .arg("--dry-run")
        .arg("--force")
        .arg("gamma")
        .assert()
        .failure()
        .stderr(str::contains("gamma"));
}

/// Bare `--force` over a manifest with no actions is not an error — there is
/// simply nothing to do.
#[test]
fn force_over_an_empty_manifest_is_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    cmd()
        .arg("--manifest")
        .arg(fixtures().join("empty.toml"))
        .arg("--cache-path")
        .arg(tmp.path().join("cache.db"))
        .arg("sync")
        .arg("--dry-run")
        .arg("--force")
        .assert()
        .success()
        .stdout(str::contains("0 action(s) would run"));
}

#[test]
fn sync_help_documents_force() {
    cmd()
        .arg("sync")
        .arg("--help")
        .assert()
        .success()
        .stdout(str::contains("--force"))
        .stdout(str::contains("FILTER"));
}
