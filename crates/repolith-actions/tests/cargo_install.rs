//! Integration tests for [`repolith_actions::cargo_install::CargoInstall`].
//!
//! Each test installs the local `tests/fixtures/hello_world` crate into a
//! fresh tempdir; no network access required. Skipped at compile time when
//! the `cargo` feature is disabled.

#![cfg(feature = "cargo")]

use repolith_actions::cargo_install::{CargoInstall, CargoSource};
use repolith_core::action::Action;
use repolith_core::types::{ActionId, BuildError, Ctx};
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("hello_world")
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

fn make_action(install_to: PathBuf, features: Vec<String>) -> CargoInstall {
    CargoInstall {
        id: ActionId("hello::cargo-install::0".to_string()),
        source: CargoSource::Path {
            path: fixture_path(),
        },
        crate_name: Some("hello-world".to_string()),
        package: None,
        profile: None,
        features,
        install_to,
        deps: vec![],
    }
}

#[tokio::test]
async fn install_from_path_produces_binary() {
    let tmp = TempDir::new().expect("tempdir");
    let action = make_action(tmp.path().to_path_buf(), vec![]);
    let out = action
        .execute(&fresh_ctx())
        .await
        .expect("cargo install must succeed");
    let bin = tmp.path().join("bin").join("hello-world");
    assert!(bin.is_file(), "expected binary at {bin:?}");
    assert!(out.stdout.contains("hello-world"));
}

#[tokio::test]
async fn input_hash_includes_features() {
    let tmp = TempDir::new().unwrap();
    let plain = make_action(tmp.path().to_path_buf(), vec![]);
    let loud = make_action(tmp.path().to_path_buf(), vec!["loud".to_string()]);

    let h_plain = plain.input_hash(&fresh_ctx()).await.unwrap();
    let h_loud = loud.input_hash(&fresh_ctx()).await.unwrap();
    assert_ne!(
        h_plain, h_loud,
        "different features must yield different input_hash"
    );

    // Determinism check.
    let h_plain_again = plain.input_hash(&fresh_ctx()).await.unwrap();
    assert_eq!(h_plain, h_plain_again);
}

#[tokio::test]
async fn cancellation_aborts() {
    let tmp = TempDir::new().unwrap();
    let action = make_action(tmp.path().to_path_buf(), vec![]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = action.execute(&ctx_with_cancel(cancel)).await;
    assert!(
        matches!(result, Err(BuildError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Regression: the hash must follow the *content* of a path source, and the
// planner must be told when the installed binary is gone (issue #73).
//
// These sit at the action level on purpose. The `source_hash` unit tests
// prove `tree_digest` works; only these prove `CargoInstall` actually uses
// it. Reverting the Path branch to hashing the path string — the original
// bug — leaves every `source_hash` test green but fails these.
// ---------------------------------------------------------------------------

/// Copy the fixture into a writable tempdir so tests can mutate it.
fn writable_fixture() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let src = fixture_path();
    for entry in std::fs::read_dir(&src).expect("read fixture") {
        let entry = entry.expect("entry");
        let dest = tmp.path().join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            std::fs::create_dir_all(&dest).expect("mkdir");
            for sub in std::fs::read_dir(entry.path()).expect("read subdir") {
                let sub = sub.expect("sub entry");
                std::fs::copy(sub.path(), dest.join(sub.file_name())).expect("copy");
            }
        } else {
            std::fs::copy(entry.path(), &dest).expect("copy");
        }
    }
    tmp
}

fn action_at(source: PathBuf, install_to: PathBuf) -> CargoInstall {
    CargoInstall {
        id: ActionId("hello::cargo-install::0".to_string()),
        source: CargoSource::Path { path: source },
        crate_name: Some("hello-world".to_string()),
        package: None,
        profile: None,
        features: vec![],
        install_to,
        deps: vec![],
    }
}

#[tokio::test]
async fn input_hash_follows_source_content_not_the_path() {
    let src = writable_fixture();
    let install = TempDir::new().expect("tempdir");
    let action = action_at(src.path().to_path_buf(), install.path().to_path_buf());

    let before = action.input_hash(&fresh_ctx()).await.expect("hash");

    // Edit a source file. The path is unchanged — only the bytes move.
    let main_rs = src.path().join("src").join("main.rs");
    let body = std::fs::read_to_string(&main_rs).expect("read main.rs");
    std::fs::write(&main_rs, format!("{body}\n// local edit\n")).expect("write");

    let after = action.input_hash(&fresh_ctx()).await.expect("hash");
    assert_ne!(
        before, after,
        "a local edit must change input_hash — hashing the path string alone \
         is what made `sync` report `up to date` on a stale binary"
    );
}

#[tokio::test]
async fn input_hash_is_stable_when_nothing_changes() {
    let src = writable_fixture();
    let install = TempDir::new().expect("tempdir");
    let action = action_at(src.path().to_path_buf(), install.path().to_path_buf());

    let a = action.input_hash(&fresh_ctx()).await.expect("hash");
    let b = action.input_hash(&fresh_ctx()).await.expect("hash");
    assert_eq!(a, b, "identical inputs must hash identically across runs");
}

#[tokio::test]
async fn input_hash_survives_a_missing_source_tree() {
    // Pre-clone grace: a sibling git-clone materializes this path later, so
    // planning must not fail — it must yield a deterministic marker.
    let install = TempDir::new().expect("tempdir");
    let action = action_at(
        PathBuf::from("/nonexistent/repolith/pre-clone"),
        install.path().to_path_buf(),
    );

    let a = action
        .input_hash(&fresh_ctx())
        .await
        .expect("must not fail");
    let b = action
        .input_hash(&fresh_ctx())
        .await
        .expect("must not fail");
    assert_eq!(a, b, "pre-clone marker must be deterministic");
}

#[tokio::test]
async fn output_present_tracks_the_installed_binary() {
    let tmp = TempDir::new().expect("tempdir");
    let action = make_action(tmp.path().to_path_buf(), vec![]);

    assert!(
        !action.output_present(&fresh_ctx()).await,
        "nothing installed yet"
    );

    action.execute(&fresh_ctx()).await.expect("install");
    assert!(
        action.output_present(&fresh_ctx()).await,
        "binary is installed"
    );

    std::fs::remove_file(tmp.path().join("bin").join("hello-world")).expect("rm");
    assert!(
        !action.output_present(&fresh_ctx()).await,
        "a deleted binary must be reported missing — this is what makes a \
         cache entry written by another machine safe to distrust"
    );
}

/// Two nodes over the same source that differ only by which package they
/// select must not share an input hash — otherwise the second silently
/// inherits the first's cached Success (issue #77).
#[tokio::test]
async fn input_hash_distinguishes_the_selected_package() {
    let src = writable_fixture();
    let install = TempDir::new().expect("tempdir");

    let mut a = action_at(src.path().to_path_buf(), install.path().to_path_buf());
    a.package = Some("pkg-a".to_string());
    let mut b = action_at(src.path().to_path_buf(), install.path().to_path_buf());
    b.package = Some("pkg-b".to_string());
    let none = action_at(src.path().to_path_buf(), install.path().to_path_buf());

    let ha = a.input_hash(&fresh_ctx()).await.expect("hash");
    let hb = b.input_hash(&fresh_ctx()).await.expect("hash");
    let hn = none.input_hash(&fresh_ctx()).await.expect("hash");

    assert_ne!(ha, hb, "different packages must hash differently");
    assert_ne!(ha, hn, "selecting a package must differ from not selecting");
}

/// The profile changes the artifact but not its install path, so
/// `output_present` cannot distinguish release from debug. Only the hash
/// can — without this, switching profiles would report `up to date` with
/// the wrong binary installed (issue #82).
#[tokio::test]
async fn input_hash_distinguishes_the_build_profile() {
    let src = writable_fixture();
    let install = TempDir::new().expect("tempdir");

    let release = action_at(src.path().to_path_buf(), install.path().to_path_buf());
    let mut dev = action_at(src.path().to_path_buf(), install.path().to_path_buf());
    dev.profile = Some("dev".to_string());
    let mut custom = action_at(src.path().to_path_buf(), install.path().to_path_buf());
    custom.profile = Some("release-lto".to_string());

    let hr = release.input_hash(&fresh_ctx()).await.expect("hash");
    let hd = dev.input_hash(&fresh_ctx()).await.expect("hash");
    let hc = custom.input_hash(&fresh_ctx()).await.expect("hash");

    assert_ne!(hr, hd, "release and dev must hash differently");
    assert_ne!(hd, hc, "two named profiles must hash differently");
}

/// A debug build must actually land a working binary — the profile flag is
/// passed through to cargo, not silently dropped.
#[tokio::test]
async fn dev_profile_installs_a_working_binary() {
    let tmp = TempDir::new().expect("tempdir");
    let mut action = make_action(tmp.path().to_path_buf(), vec![]);
    action.profile = Some("dev".to_string());

    action
        .execute(&fresh_ctx())
        .await
        .expect("install --profile dev");
    let bin = tmp.path().join("bin").join("hello-world");
    assert!(bin.is_file(), "expected binary at {bin:?}");
    assert!(action.output_present(&fresh_ctx()).await);
}
