//! Backend-parametric contract suite — the SAME assertions run against
//! every [`Cache`] implementation, so a new backend cannot silently
//! weaken the contract.
//!
//! `SQLite` runs everywhere. Neo4j tests are `#[ignore]` by default and
//! need a live server plus the `REPOLITH_NEO4J_*` env vars:
//!
//! ```bash
//! docker run -d --rm -p7687:7687 -e NEO4J_AUTH=neo4j/testpassword neo4j:5
//! REPOLITH_NEO4J_URI=bolt://localhost:7687 REPOLITH_NEO4J_USER=neo4j \
//!   REPOLITH_NEO4J_PASS=testpassword \
//!   cargo test -p repolith-cache --features neo4j -- --ignored
//! ```

use repolith_cache::{Cache, SqliteCache};
use repolith_core::types::{ActionId, BuildError, BuildEvent, Sha256};

fn success(id: &str, ms: u64) -> BuildEvent {
    BuildEvent::Success {
        id: ActionId(id.to_string()),
        input: Sha256([3; 32]),
        output: Sha256([4; 32]),
        ms,
    }
}

fn failed(id: &str) -> BuildEvent {
    BuildEvent::Failed {
        id: ActionId(id.to_string()),
        input: Sha256([5; 32]),
        error: BuildError::Io("disk full".to_string()),
        ms: 7,
    }
}

/// The contract every backend must satisfy. `ns` isolates repeated runs
/// against persistent servers (Neo4j keeps state between test runs).
async fn run_contract_suite(cache: &mut dyn Cache, ns: &str) {
    let id = |s: &str| ActionId(format!("{ns}::{s}"));

    // 1. Empty cache: no event.
    assert!(
        cache.last_build(&id("absent")).await.is_none(),
        "unknown id must return None"
    );

    // 2. Success roundtrip: every field survives.
    cache
        .record(success(&id("ok").0, 42))
        .await
        .expect("record success");
    match cache.last_build(&id("ok")).await.expect("event stored") {
        BuildEvent::Success {
            input, output, ms, ..
        } => {
            assert_eq!(input, Sha256([3; 32]));
            assert_eq!(output, Sha256([4; 32]));
            assert_eq!(ms, 42);
        }
        BuildEvent::Failed { .. } => panic!("expected Success"),
    }

    // 3. Failed roundtrip: the typed error survives (not stringified).
    cache
        .record(failed(&id("ko").0))
        .await
        .expect("record failed");
    match cache.last_build(&id("ko")).await.expect("event stored") {
        BuildEvent::Failed { error, .. } => {
            assert!(
                matches!(&error, BuildError::Io(m) if m == "disk full"),
                "typed error must roundtrip, got: {error:?}"
            );
        }
        BuildEvent::Success { .. } => panic!("expected Failed"),
    }

    // 4. Overwrite: last_build returns the LATEST event for an id.
    cache
        .record(success(&id("ok").0, 99))
        .await
        .expect("overwrite");
    match cache.last_build(&id("ok")).await.expect("event stored") {
        BuildEvent::Success { ms, .. } => assert_eq!(ms, 99, "latest event wins"),
        BuildEvent::Failed { .. } => panic!("expected Success"),
    }

    // 5. A Failed overwrite of a Success (status flip) is visible.
    cache
        .record(failed(&id("ok").0))
        .await
        .expect("flip to failed");
    assert!(
        matches!(
            cache.last_build(&id("ok")).await,
            Some(BuildEvent::Failed { .. })
        ),
        "status flip must be visible"
    );

    // 6. Batch: every event lands and is readable afterwards.
    let batch: Vec<BuildEvent> = (0..5)
        .map(|i| success(&id(&format!("b{i}")).0, i))
        .collect();
    cache.record_batch(batch).await.expect("record_batch");
    for i in 0..5u64 {
        match cache.last_build(&id(&format!("b{i}"))).await {
            Some(BuildEvent::Success { ms, .. }) => assert_eq!(ms, i),
            other => panic!("batch event b{i} missing or wrong: {other:?}"),
        }
    }

    // 7. Empty batch is a no-op, not an error.
    cache.record_batch(vec![]).await.expect("empty batch ok");
}

#[tokio::test]
async fn sqlite_satisfies_the_contract() {
    let mut cache = SqliteCache::in_memory().expect("in-memory sqlite");
    run_contract_suite(&mut cache, "run").await;
}

#[tokio::test]
async fn sqlite_file_backed_satisfies_the_contract() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cache = SqliteCache::open(dir.path().join("contract.db")).expect("open");
    run_contract_suite(&mut cache, "run").await;
}

#[cfg(feature = "neo4j")]
mod neo4j_contract {
    use super::run_contract_suite;
    use repolith_cache::{Neo4jCache, Neo4jConfig};

    /// Needs a live server — see the module docs for the docker one-liner.
    #[tokio::test]
    #[ignore = "requires a live Neo4j server + REPOLITH_NEO4J_* env vars"]
    async fn neo4j_satisfies_the_contract() {
        let cfg = Neo4jConfig::from_env().expect("REPOLITH_NEO4J_* env vars");
        let mut cache = Neo4jCache::connect(&cfg).await.expect("connect");
        // Nanosecond namespace so repeated runs against a persistent
        // server never collide with stale data.
        let ns = format!(
            "contract-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        );
        run_contract_suite(&mut cache, &ns).await;
    }
}
