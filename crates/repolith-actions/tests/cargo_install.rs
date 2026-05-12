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
