//! Integration tests for the [`repolith_core::manifest`] module.

use repolith_core::manifest::{ActionEntry, Manifest, ManifestError};

const MINIMAL: &str = include_str!("fixtures/manifest_minimal.toml");
const INVALID_DUP: &str = include_str!("fixtures/manifest_invalid_dup.toml");
const INVALID_SCHEMA: &str = include_str!("fixtures/manifest_invalid_schema.toml");
const NO_SOURCE: &str = include_str!("fixtures/manifest_no_source.toml");
const FULL: &str = include_str!("fixtures/manifest_full.toml");
/// The user-facing example committed at the repo root must always parse —
/// regression-locks the docs against drift in the manifest schema.
const EXAMPLE: &str = include_str!("../../../repolith.toml.example");

#[test]
fn test_parse_minimal() {
    let m = Manifest::from_toml(MINIMAL).expect("minimal manifest must parse");
    assert_eq!(m.orchestrator.schema_version, "0.1");
    assert_eq!(m.orchestrator.name, "test");
    assert_eq!(m.nodes.len(), 1);
    assert_eq!(m.nodes[0].id, "a");
    assert_eq!(m.nodes[0].actions.len(), 1);
    assert!(matches!(m.nodes[0].actions[0], ActionEntry::GitClone));
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
}

#[test]
fn test_example_fixture_parses() {
    // The committed `repolith.toml.example` at the repo root must always
    // parse + validate — protects the README's quick-start from rotting.
    let m = Manifest::from_toml(EXAMPLE).expect("repolith.toml.example must parse");
    assert!(
        !m.orchestrator.name.is_empty(),
        "example manifest must set an orchestrator name"
    );
    assert!(
        !m.nodes.is_empty(),
        "example must showcase at least one node"
    );
    // Action ids must follow the documented `{node.id}::{kind}::{idx}` format.
    for id in m.action_ids() {
        let segments: Vec<&str> = id.0.split("::").collect();
        assert_eq!(
            segments.len(),
            3,
            "action id `{}` must have 3 ::-separated segments",
            id.0
        );
    }
}

// ---------------------------------------------------------------------------
// Argument-injection (CWE-88) negative tests — every user-controlled string
// that ultimately reaches a `git` / `cargo` subprocess must be rejected at
// parse time when it would be interpreted as a CLI flag or a list separator.
// ---------------------------------------------------------------------------

fn parse_err(toml: &str) -> ManifestError {
    Manifest::from_toml(toml).expect_err("manifest must fail validation")
}

#[test]
fn test_reject_url_with_leading_dash() {
    let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = "a"
git = "--upload-pack=touch /tmp/pwn"
path = "./a"

  [[node.action]]
  kind = "git-clone"
"#;
    match parse_err(toml) {
        ManifestError::InvalidUrl { node, reason } => {
            assert_eq!(node, "a");
            assert!(
                reason.contains("starts with `-`"),
                "expected leading-dash reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidUrl, got {other:?}"),
    }
}

#[test]
fn test_reject_url_with_unknown_scheme() {
    let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = "a"
git = "ext::sh -c id"
path = "./a"

  [[node.action]]
  kind = "git-clone"
"#;
    match parse_err(toml) {
        ManifestError::InvalidUrl { node, reason } => {
            assert_eq!(node, "a");
            assert!(
                reason.contains("scheme not in allowlist"),
                "expected scheme reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidUrl, got {other:?}"),
    }
}

#[test]
fn test_reject_crate_name_with_leading_dash() {
    let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = "a"
path = "./a"

  [[node.action]]
  kind = "cargo-install"
  crate = "--config=net.git-fetch-with-cli=true"
"#;
    match parse_err(toml) {
        ManifestError::InvalidArg { node, reason } => {
            assert_eq!(node, "a");
            assert!(
                reason.contains("starts with `-`"),
                "expected leading-dash reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidArg, got {other:?}"),
    }
}

#[test]
fn test_reject_feature_with_comma_injection() {
    let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = "a"
path = "./a"

  [[node.action]]
  kind = "cargo-install"
  crate = "ok"
  features = ["loud,--target-dir=/etc"]
"#;
    match parse_err(toml) {
        ManifestError::InvalidArg { node, reason } => {
            assert_eq!(node, "a");
            assert!(
                reason.contains("contains `,`"),
                "expected comma-injection reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidArg, got {other:?}"),
    }
}

#[test]
fn test_reject_feature_with_leading_dash() {
    let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = "a"
path = "./a"

  [[node.action]]
  kind = "cargo-install"
  crate = "ok"
  features = ["--target-dir=/etc"]
"#;
    match parse_err(toml) {
        ManifestError::InvalidArg { node, reason } => {
            assert_eq!(node, "a");
            assert!(
                reason.contains("starts with `-`"),
                "expected leading-dash reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidArg, got {other:?}"),
    }
}

#[test]
fn test_reject_url_with_newline() {
    let toml = "
[orchestrator]
schema_version = \"0.1\"
name = \"test\"

[[node]]
id = \"a\"
git = \"https://example.com/r.git\\n--exec=evil\"
path = \"./a\"

  [[node.action]]
  kind = \"git-clone\"
";
    match parse_err(toml) {
        ManifestError::InvalidUrl { node, reason } => {
            assert_eq!(node, "a");
            assert!(
                reason.contains("control character"),
                "expected control-char reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidUrl, got {other:?}"),
    }
}

#[test]
fn test_reject_ssh_url_with_leading_dash_in_userinfo() {
    // `ssh://-oProxyCommand=evil@host/r.git` parses with user
    // `-oProxyCommand=evil`. Git/ssh has historically been tricked into
    // treating that as a flag — must reject at manifest parse time.
    let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = "a"
git = "ssh://-oProxyCommand=evil@host/r.git"
path = "./a"

  [[node.action]]
  kind = "git-clone"
"#;
    match parse_err(toml) {
        ManifestError::InvalidUrl { node, reason } => {
            assert_eq!(node, "a");
            assert!(
                reason.contains("userinfo") && reason.contains("`-`"),
                "expected userinfo-dash reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidUrl, got {other:?}"),
    }
}

#[test]
fn test_reject_ssh_url_with_leading_dash_in_host() {
    let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = "a"
git = "ssh://-evilhost/r.git"
path = "./a"

  [[node.action]]
  kind = "git-clone"
"#;
    match parse_err(toml) {
        ManifestError::InvalidUrl { node, reason } => {
            assert_eq!(node, "a");
            assert!(
                reason.contains("host") && reason.contains("`-`"),
                "expected host-dash reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidUrl, got {other:?}"),
    }
}

#[test]
fn test_reject_https_url_with_leading_dash_in_path() {
    let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = "a"
git = "https://host.example/-evil/r.git"
path = "./a"

  [[node.action]]
  kind = "git-clone"
"#;
    match parse_err(toml) {
        ManifestError::InvalidUrl { node, reason } => {
            assert_eq!(node, "a");
            assert!(
                reason.contains("path segment") && reason.contains("`-`"),
                "expected path-segment-dash reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidUrl, got {other:?}"),
    }
}

#[test]
fn test_reject_scp_url_with_leading_dash_in_host() {
    let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = "a"
git = "git@-evil:org/repo.git"
path = "./a"

  [[node.action]]
  kind = "git-clone"
"#;
    match parse_err(toml) {
        ManifestError::InvalidUrl { node, reason } => {
            assert_eq!(node, "a");
            assert!(
                reason.contains("host") && reason.contains("`-`"),
                "expected host-dash reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidUrl, got {other:?}"),
    }
}

#[test]
fn test_accept_valid_scp_url() {
    let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = "a"
git = "git@github.com:anatta-rs/repolith.git"
path = "./a"

  [[node.action]]
  kind = "git-clone"
"#;
    Manifest::from_toml(toml).expect("valid SCP-style git URL must parse");
}
