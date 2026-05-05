//! Integration tests for the [`repolith_core::manifest`] module.

use repolith_core::manifest::{ActionEntry, Direction, Manifest, ManifestError};

const MINIMAL: &str = include_str!("fixtures/manifest_minimal.toml");
const INVALID_DUP: &str = include_str!("fixtures/manifest_invalid_dup.toml");
const INVALID_SCHEMA: &str = include_str!("fixtures/manifest_invalid_schema.toml");
const NO_SOURCE: &str = include_str!("fixtures/manifest_no_source.toml");
const FULL: &str = include_str!("fixtures/manifest_full.toml");

#[test]
fn test_parse_minimal() {
    let m = Manifest::from_toml(MINIMAL).expect("minimal manifest must parse");
    assert_eq!(m.orchestrator.schema_version, "0.1");
    assert_eq!(m.orchestrator.name, "test");
    assert_eq!(m.nodes.len(), 1);
    assert_eq!(m.nodes[0].id, "a");
    assert_eq!(m.nodes[0].actions.len(), 1);
    assert!(matches!(m.nodes[0].actions[0], ActionEntry::GitClone));
    assert!(m.attached.is_empty());
}

#[test]
fn test_reject_schema_mismatch() {
    let err = Manifest::from_toml(INVALID_SCHEMA).unwrap_err();
    match err {
        ManifestError::SchemaMismatch { got } => assert_eq!(got, "0.2"),
        other => panic!("expected SchemaMismatch, got {other:?}"),
    }
}

#[test]
fn test_reject_duplicate_node_id() {
    let err = Manifest::from_toml(INVALID_DUP).unwrap_err();
    match err {
        ManifestError::DuplicateNodeId(id) => assert_eq!(id, "a"),
        other => panic!("expected DuplicateNodeId, got {other:?}"),
    }
}

#[test]
fn test_reject_node_missing_source() {
    let err = Manifest::from_toml(NO_SOURCE).unwrap_err();
    match err {
        ManifestError::NodeMissingSource(id) => assert_eq!(id, "orphan"),
        other => panic!("expected NodeMissingSource, got {other:?}"),
    }
}

#[test]
fn test_roundtrip() {
    let original = Manifest::from_toml(FULL).expect("full fixture must parse");
    let serialized = toml::to_string(&original).expect("serialize must succeed");
    let reparsed = Manifest::from_toml(&serialized).expect("reparse must succeed");
    assert_eq!(original, reparsed);
}

#[test]
fn test_action_ids_format() {
    let m = Manifest::from_toml(FULL).expect("full fixture must parse");
    let ids = m.action_ids();
    // 2 nodes × 2 actions = 4 ids
    assert_eq!(ids.len(), 4);
    assert_eq!(ids[0].0, "anatta-core::git-clone::0");
    assert_eq!(ids[1].0, "anatta-core::cargo-install::1");
    assert_eq!(ids[2].0, "anatta-cli::git-clone::0");
    assert_eq!(ids[3].0, "anatta-cli::cargo-install::1");

    // attached entry direction is preserved
    assert_eq!(m.attached.len(), 1);
    assert_eq!(m.attached[0].direction, Direction::Inbound);
}
