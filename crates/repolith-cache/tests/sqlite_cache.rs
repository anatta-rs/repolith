//! Integration tests for [`repolith_cache::sqlite::SqliteCache`].

use repolith_cache::{Cache, SqliteCache};
use repolith_core::types::{ActionId, BuildError, BuildEvent, Sha256};

#[tokio::test]
async fn record_then_last_build() {
    let mut cache = SqliteCache::in_memory().expect("in-memory cache must open");
    let id = ActionId("test".to_string());
    let input = Sha256([1; 32]);
    let output = Sha256([2; 32]);
    cache
        .record(BuildEvent::Success {
            id: id.clone(),
            input,
            output,
            ms: 42,
        })
        .await
        .unwrap();
    let got = cache.last_build(&id).await.expect("event must be present");
    match got {
        BuildEvent::Success {
            id: got_id,
            input: got_input,
            output: got_output,
            ms,
        } => {
            assert_eq!(got_id, id);
            assert_eq!(got_input, input);
            assert_eq!(got_output, output);
            assert_eq!(ms, 42);
        }
        BuildEvent::Failed { .. } => panic!("expected Success, got Failed"),
    }
}

#[tokio::test]
async fn missing_returns_none() {
    let cache = SqliteCache::in_memory().expect("in-memory cache must open");
    assert!(
        cache
            .last_build(&ActionId("absent".to_string()))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn replaces_on_second_record() {
    let mut cache = SqliteCache::in_memory().unwrap();
    let id = ActionId("dup".to_string());
    let first = BuildEvent::Success {
        id: id.clone(),
        input: Sha256([0; 32]),
        output: Sha256([0xaa; 32]),
        ms: 10,
    };
    let second = BuildEvent::Success {
        id: id.clone(),
        input: Sha256([1; 32]),
        output: Sha256([0xbb; 32]),
        ms: 20,
    };
    cache.record(first).await.unwrap();
    cache.record(second).await.unwrap();
    let got = cache.last_build(&id).await.expect("present");
    match got {
        BuildEvent::Success { output, ms, .. } => {
            assert_eq!(output, Sha256([0xbb; 32]), "second record must win");
            assert_eq!(ms, 20);
        }
        BuildEvent::Failed { .. } => panic!("expected Success, got Failed"),
    }
}

#[tokio::test]
async fn failed_event_roundtrip() {
    let mut cache = SqliteCache::in_memory().unwrap();
    let id = ActionId("flaky".to_string());
    let original_error = BuildError::CommandFailed {
        exit_code: 7,
        stderr: "boom".to_string(),
    };
    cache
        .record(BuildEvent::Failed {
            id: id.clone(),
            input: Sha256([3; 32]),
            error: original_error.clone(),
            ms: 99,
        })
        .await
        .unwrap();
    let got = cache.last_build(&id).await.expect("failed event present");
    match got {
        BuildEvent::Failed {
            error, ms, input, ..
        } => {
            assert_eq!(input, Sha256([3; 32]));
            assert_eq!(ms, 99);
            if let BuildError::CommandFailed { exit_code, stderr } = error {
                assert_eq!(exit_code, 7);
                assert_eq!(stderr, "boom");
            } else {
                panic!("expected CommandFailed variant after roundtrip");
            }
        }
        BuildEvent::Success { .. } => panic!("expected Failed, got Success"),
    }
}

#[tokio::test]
async fn open_creates_parent_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let nested = tmp.path().join("a").join("b").join("c").join("cache.db");
    assert!(!nested.parent().unwrap().exists(), "precondition");
    let mut cache = SqliteCache::open(&nested).expect("open must create parents");
    assert!(nested.parent().unwrap().is_dir());
    assert!(nested.is_file());
    // Smoke a write to confirm the schema applied.
    cache
        .record(BuildEvent::Success {
            id: ActionId("smoke".to_string()),
            input: Sha256([0; 32]),
            output: Sha256([0; 32]),
            ms: 1,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn record_batch_persists_all_events() {
    // The transactional override of `record_batch` should persist every
    // event when called with a non-empty batch, leaving all of them
    // visible to subsequent `last_build` calls. The default-impl loop
    // would also pass this test — what we'd really like to assert is
    // atomicity on failure, but we have no failing-INSERT path in M1
    // (INSERT OR REPLACE never fails on conflict). This test at least
    // locks the happy-path behavior.
    let mut cache = SqliteCache::in_memory().unwrap();
    let events = vec![
        BuildEvent::Success {
            id: ActionId("a".into()),
            input: Sha256([1; 32]),
            output: Sha256([2; 32]),
            ms: 10,
        },
        BuildEvent::Success {
            id: ActionId("b".into()),
            input: Sha256([3; 32]),
            output: Sha256([4; 32]),
            ms: 20,
        },
        BuildEvent::Failed {
            id: ActionId("c".into()),
            input: Sha256([5; 32]),
            error: BuildError::Cancelled,
            ms: 30,
        },
    ];
    cache.record_batch(events).await.unwrap();
    assert!(matches!(
        cache.last_build(&ActionId("a".into())).await,
        Some(BuildEvent::Success { ms: 10, .. })
    ));
    assert!(matches!(
        cache.last_build(&ActionId("b".into())).await,
        Some(BuildEvent::Success { ms: 20, .. })
    ));
    assert!(matches!(
        cache.last_build(&ActionId("c".into())).await,
        Some(BuildEvent::Failed { ms: 30, .. })
    ));
}

#[tokio::test]
async fn record_batch_empty_is_noop() {
    let mut cache = SqliteCache::in_memory().unwrap();
    cache.record_batch(vec![]).await.unwrap();
    assert!(
        cache
            .last_build(&ActionId("anything".into()))
            .await
            .is_none(),
        "empty batch must not change cache state"
    );
}
