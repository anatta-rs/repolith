//! End-to-end smoke tests: shells out to the `repolith` binary against a
//! real fixture (manifest + git repo + buildable crates) and asserts the
//! whole pipeline behaves correctly.
//!
//! Each test owns a fresh tempdir + fresh cache so they can run in parallel
//! without sharing state. Total runtime budget is ~30s — the longest test
//! does two `cargo install --path` of a tiny crate.

mod common;

use common::{
    add_commit_to_node_a, run_repolith, setup_smoke_fixture, setup_smoke_fixture_failfast,
};

fn assert_stdout_contains(out: &std::process::Output, needle: &str) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains(needle),
        "expected stdout to contain `{needle}`\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}

fn count_lines_starting_with(out: &std::process::Output, prefix: &str) -> usize {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.trim_start().starts_with(prefix))
        .count()
}

#[test]
fn first_run_3_successes() {
    let env = setup_smoke_fixture();
    let out = run_repolith(&env, &["sync"]);
    assert!(
        out.status.success(),
        "sync must succeed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let oks = count_lines_starting_with(&out, "OK   ");
    assert_eq!(
        oks,
        3,
        "expected 3 OK lines, got {oks}\nstdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn second_run_zero_stale() {
    let env = setup_smoke_fixture();
    // Warm up the cache.
    let first = run_repolith(&env, &["sync"]);
    assert!(first.status.success(), "first sync must succeed");

    // Second invocation: dry-run + explain so we don't actually re-execute.
    let out = run_repolith(&env, &["sync", "--dry-run", "--explain"]);
    assert!(out.status.success(), "second sync --dry-run must succeed");
    assert_stdout_contains(&out, "0 action(s) would run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("NoCachedBuild"),
        "no action should be stale on a cache hit:\n{stdout}"
    );
}

#[test]
fn cascade_node_a_changed() {
    let env = setup_smoke_fixture();
    let first = run_repolith(&env, &["sync"]);
    assert!(first.status.success(), "first sync must succeed");

    add_commit_to_node_a(&env, "feat: bump");

    let out = run_repolith(&env, &["sync", "--dry-run", "--explain"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("a::git-clone::0"),
        "node-a git-clone action must be flagged stale:\n{stdout}"
    );
    assert!(
        stdout.contains("InputHashChanged"),
        "stale reason must be InputHashChanged:\n{stdout}"
    );
    // b and c are independent of a in this manifest (no cross-node deps in
    // the M1 schema), so they must remain cache hits.
    assert!(
        !stdout.contains("b::cargo-install"),
        "b should not be stale:\n{stdout}"
    );
    assert!(
        !stdout.contains("c::cargo-install"),
        "c should not be stale:\n{stdout}"
    );
    assert_stdout_contains(&out, "1 action(s) would run");
}

#[test]
fn failfast_halts_layer_two() {
    let env = setup_smoke_fixture_failfast();
    let out = run_repolith(&env, &["sync"]);
    assert!(
        !out.status.success(),
        "sync must fail when the unreachable git-clone errors"
    );
    let fails = count_lines_starting_with(&out, "FAIL ");
    let oks = count_lines_starting_with(&out, "OK   ");
    assert_eq!(fails, 1, "expected exactly 1 FAIL line, got {fails}");
    assert_eq!(
        oks, 0,
        "cargo-install on layer 2 must be skipped, got {oks} OK lines"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("failing-node::cargo-install::0"),
        "FAIL line must reference the first action:\n{stdout}"
    );
    assert!(
        !stdout.contains("failing-node::cargo-install::1"),
        "second action must be skipped (no event):\n{stdout}"
    );
}
