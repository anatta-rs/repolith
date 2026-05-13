//! Integration tests for [`repolith_actions::git_clone::GitClone`].
//!
//! Uses a local bare repo as the "remote" so no network access is required.
//! Skipped at compile time when the `git` feature is disabled.

#![cfg(feature = "git")]

use repolith_actions::git_clone::GitClone;
use repolith_core::action::Action;
use repolith_core::types::{ActionId, BuildError, Ctx};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::Duration;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Fixture helpers — synchronous; no async needed for setup.
// ---------------------------------------------------------------------------

fn run_git(cwd: &Path, args: &[&str]) {
    let output = StdCommand::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("git binary must be on PATH");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Build a fresh bare repo with one commit. Returns (`TempDir` guard, bare path,
/// initial HEAD sha1).
fn make_bare_repo() -> (TempDir, PathBuf, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("origin.git");
    let work = tmp.path().join("seed");

    StdCommand::new("git")
        .args(["init", "--bare", bare.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    run_git(tmp.path(), &["init", work.to_str().unwrap()]);

    // Configure identity locally so commits work in CI environments without globals.
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "user.name", "Test"]);
    run_git(&work, &["config", "commit.gpgsign", "false"]);

    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    run_git(&work, &["add", "README.md"]);
    run_git(&work, &["commit", "-m", "initial"]);
    // Push the default branch as HEAD on the bare remote.
    run_git(&work, &["branch", "-M", "main"]);
    run_git(&work, &["push", bare.to_str().unwrap(), "main:main"]);
    // Make `main` the bare repo's HEAD so `ls-remote HEAD` resolves it.
    run_git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let head_out = StdCommand::new("git")
        .current_dir(&work)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let head_sha = String::from_utf8(head_out.stdout)
        .unwrap()
        .trim()
        .to_string();

    (tmp, bare, head_sha)
}

fn ctx_with_cancel(cancel: CancellationToken) -> Ctx {
    Ctx {
        cancel,
        workdir: PathBuf::from("/tmp"),
        env: HashMap::new(),
    }
}

fn fresh_ctx() -> Ctx {
    ctx_with_cancel(CancellationToken::new())
}

fn add_commit(work: &Path) -> String {
    std::fs::write(work.join("more.txt"), "second\n").unwrap();
    run_git(work, &["add", "more.txt"]);
    run_git(work, &["commit", "-m", "second"]);
    let head = StdCommand::new("git")
        .current_dir(work)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(head.stdout).unwrap().trim().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn input_hash_is_stable_and_url_dependent() {
    let (_guard, bare, _) = make_bare_repo();
    let url = format!("file://{}", bare.display());

    let action = GitClone {
        id: ActionId("git-a".into()),
        repo_url: url.clone(),
        path: PathBuf::from("/tmp/never-clone-me"),
        deps: vec![],
    };
    let h1 = action.input_hash(&fresh_ctx()).await.unwrap();
    let h2 = action.input_hash(&fresh_ctx()).await.unwrap();
    assert_eq!(h1, h2, "input_hash must be deterministic");

    // Different URL → different hash.
    let action_other_url = GitClone {
        id: ActionId("git-a".into()),
        repo_url: format!("{url}-fake"),
        path: PathBuf::from("/tmp/never-clone-me"),
        deps: vec![],
    };
    let other = action_other_url.input_hash(&fresh_ctx()).await;
    // Other URL is unreachable → UpstreamUnreachable, not equality. Good — confirms it actually hit the remote.
    assert!(matches!(other, Err(BuildError::UpstreamUnreachable(_))));
}

#[tokio::test]
async fn first_execute_clones() {
    let (guard, bare, head_sha) = make_bare_repo();
    let dest = guard.path().join("dest");
    let url = format!("file://{}", bare.display());

    let action = GitClone {
        id: ActionId("clone-once".into()),
        repo_url: url,
        path: dest.clone(),
        deps: vec![],
    };
    let out = action.execute(&fresh_ctx()).await.unwrap();
    assert!(dest.join(".git").is_dir(), "expected .git after clone");
    assert!(out.stdout.contains(&head_sha));
}

#[tokio::test]
async fn second_execute_updates() {
    let (guard, bare, head_sha) = make_bare_repo();
    let dest = guard.path().join("dest");
    let url = format!("file://{}", bare.display());

    let action = GitClone {
        id: ActionId("clone-then-update".into()),
        repo_url: url,
        path: dest.clone(),
        deps: vec![],
    };

    // First execute — clone.
    let first = action.execute(&fresh_ctx()).await.unwrap();
    assert!(first.stdout.contains(&head_sha));

    // Push a new commit to the bare repo from a separate working tree.
    let updater = guard.path().join("updater");
    run_git(
        guard.path(),
        &["clone", bare.to_str().unwrap(), updater.to_str().unwrap()],
    );
    run_git(&updater, &["config", "user.email", "test@example.com"]);
    run_git(&updater, &["config", "user.name", "Test"]);
    run_git(&updater, &["config", "commit.gpgsign", "false"]);
    let new_head = add_commit(&updater);
    run_git(&updater, &["push", "origin", "HEAD:main"]);

    // Second execute — should fetch + reset to the new HEAD.
    let second = action.execute(&fresh_ctx()).await.unwrap();
    assert!(
        second.stdout.contains(&new_head),
        "expected output to contain new HEAD {new_head}, got {}",
        second.stdout
    );
    assert_ne!(first.output_hash, second.output_hash);
}

#[tokio::test]
async fn cancellation_aborts() {
    let (guard, bare, _) = make_bare_repo();
    let dest = guard.path().join("dest");
    let url = format!("file://{}", bare.display());
    let cancel = CancellationToken::new();

    let action = GitClone {
        id: ActionId("cancel-me".into()),
        repo_url: url,
        path: dest,
        deps: vec![],
    };
    // Cancel before the call so the select! sees the token immediately.
    cancel.cancel();
    let result = action.execute(&ctx_with_cancel(cancel)).await;
    assert!(
        matches!(result, Err(BuildError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
}

#[tokio::test]
async fn unreachable_url_yields_upstream_unreachable() {
    // file:// URL pointing to a non-existent directory — git ls-remote fails fast.
    let action = GitClone {
        id: ActionId("nope".into()),
        repo_url: "file:///definitely/does/not/exist/repo.git".to_string(),
        path: PathBuf::from("/tmp/never-clone-me-2"),
        deps: vec![],
    };

    // Use a small timeout safety net so we never hang in CI.
    let ctx = fresh_ctx();
    let result = tokio::time::timeout(Duration::from_secs(10), action.input_hash(&ctx))
        .await
        .expect("ls-remote must complete within 10s");
    assert!(matches!(result, Err(BuildError::UpstreamUnreachable(_))));
}
