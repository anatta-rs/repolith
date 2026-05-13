//! End-to-end tests for the `repolith` CLI via `assert_cmd`.
//!
//! Each test invokes the binary as a subprocess against a fixture manifest.
//! `--dry-run` flows never write to the cache file — but we still point
//! `--cache-path` at a tempdir to keep tests fully isolated.

use assert_cmd::Command;
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
        .stdout(str::contains("NoCachedBuild"));
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
