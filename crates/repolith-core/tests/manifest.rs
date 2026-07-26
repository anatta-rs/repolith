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

// ---------------------------------------------------------------------------
// Node-id path-traversal — the default clone-path derivation is `./{id}`,
// so any path-separator or `..` in `id` lets a hostile manifest reach
// outside the workspace.
// ---------------------------------------------------------------------------

fn parse_err_for_id(node_id: &str) -> ManifestError {
    // Use a TOML literal-string (single quotes) so `\` in `node_id`
    // doesn't get interpreted as an escape sequence by the TOML parser.
    let toml = format!(
        r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = '{node_id}'
git = "https://example.com/r.git"
path = "./a"

  [[node.action]]
  kind = "git-clone"
"#
    );
    Manifest::from_toml(&toml).expect_err("manifest must fail validation")
}

#[test]
fn test_reject_node_id_with_slash() {
    match parse_err_for_id("foo/bar") {
        ManifestError::InvalidArg { node, reason } => {
            assert_eq!(node, "foo/bar");
            assert!(
                reason.contains("path separator"),
                "expected path-separator reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidArg, got {other:?}"),
    }
}

#[test]
fn test_reject_node_id_with_backslash() {
    match parse_err_for_id(r"foo\bar") {
        ManifestError::InvalidArg { node, reason } => {
            assert_eq!(node, r"foo\bar");
            assert!(
                reason.contains("path separator"),
                "expected path-separator reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidArg, got {other:?}"),
    }
}

#[test]
fn test_reject_node_id_dotdot() {
    match parse_err_for_id("..") {
        ManifestError::InvalidArg { node, reason } => {
            assert_eq!(node, "..");
            assert!(
                reason.contains("path-traversal"),
                "expected path-traversal reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidArg, got {other:?}"),
    }
}

#[test]
fn test_reject_node_id_dot() {
    match parse_err_for_id(".") {
        ManifestError::InvalidArg { node, reason } => {
            assert_eq!(node, ".");
            assert!(
                reason.contains("path-traversal"),
                "expected path-traversal reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidArg, got {other:?}"),
    }
}

#[test]
fn test_accept_node_id_with_inner_dotdot() {
    // `foo..bar` is a legal directory name; only path separators + literal
    // `.`/`..` are dangerous.
    let toml = r#"
[orchestrator]
schema_version = "0.1"
name = "test"

[[node]]
id = "foo..bar"
git = "https://example.com/r.git"
path = "./a"

  [[node.action]]
  kind = "git-clone"
"#;
    Manifest::from_toml(toml).expect("inner-dotdot node id must parse");
}

#[test]
fn test_reject_node_id_with_colon() {
    // Windows drive-letter bypass: `C:foo` resolves to a drive-relative
    // path on Windows and escapes the workspace. The `:` rejection also
    // covers alternate-data-stream selectors (`name:stream`).
    match parse_err_for_id("C:foo") {
        ManifestError::InvalidArg { node, reason } => {
            assert_eq!(node, "C:foo");
            assert!(
                reason.contains("path separator") && reason.contains(':'),
                "expected colon-separator reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidArg, got {other:?}"),
    }
}

#[test]
fn test_reject_node_id_with_nul() {
    // NUL byte truncates at the syscall layer — `./foo\0bar` opens
    // `./foo`, not what the manifest declared. Reject explicitly.
    let toml = "
[orchestrator]
schema_version = \"0.1\"
name = \"test\"

[[node]]
id = \"foo\\u0000bar\"
git = \"https://example.com/r.git\"
path = \"./a\"

  [[node.action]]
  kind = \"git-clone\"
";
    match Manifest::from_toml(toml).expect_err("manifest must fail validation") {
        ManifestError::InvalidArg { reason, .. } => {
            assert!(
                reason.contains("control character"),
                "expected control-char reason, got: {reason}"
            );
        }
        other => panic!("expected InvalidArg, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// docker action — parse + two-stage containment (lexical half, issue #61)
// ---------------------------------------------------------------------------

const DOCKER_NODE_HEADER: &str = "
[orchestrator]
schema_version = \"0.1\"
name = \"test\"

[[node]]
id = \"app\"
git = \"https://example.com/app.git\"
path = \"./app\"
";

fn docker_manifest(action_body: &str) -> String {
    format!("{DOCKER_NODE_HEADER}\n  [[node.action]]\n  kind = \"docker\"\n{action_body}")
}

fn expect_invalid_arg(toml: &str, needle: &str) {
    match Manifest::from_toml(toml).expect_err("manifest must fail validation") {
        ManifestError::InvalidArg { node, reason } => {
            assert_eq!(node, "app");
            assert!(reason.contains(needle), "expected `{needle}` in: {reason}");
        }
        other => panic!("expected InvalidArg, got {other:?}"),
    }
}

#[test]
fn test_docker_parses_with_all_fields() {
    let m = Manifest::from_toml(&docker_manifest(
        "  tag = \"app:latest\"\n  dockerfile = \"build/Dockerfile\"\n  context = \"build\"\n",
    ))
    .expect("valid docker action must parse");
    assert_eq!(m.action_ids()[0].0, "app::docker::0");
}

#[test]
fn test_docker_defaults_dockerfile_and_context() {
    let m = Manifest::from_toml(&docker_manifest("  tag = \"app:latest\"\n"))
        .expect("tag-only docker action must parse");
    match &m.nodes[0].actions[0] {
        ActionEntry::Docker {
            dockerfile,
            context,
            ..
        } => {
            assert!(dockerfile.is_none());
            assert!(context.is_none());
        }
        other => panic!("expected Docker, got {other:?}"),
    }
}

#[test]
fn test_docker_rejects_empty_tag() {
    expect_invalid_arg(&docker_manifest("  tag = \"\"\n"), "`tag` is empty");
}

#[test]
fn test_docker_rejects_tag_with_leading_dash() {
    // `-t --network=host` style injection through the tag value.
    expect_invalid_arg(
        &docker_manifest("  tag = \"--network=host\"\n"),
        "starts with `-`",
    );
}

#[test]
fn test_docker_rejects_tag_outside_charset() {
    expect_invalid_arg(
        &docker_manifest("  tag = \"app latest\"\n"),
        "allowed charset",
    );
}

#[test]
fn test_docker_rejects_dockerfile_traversal() {
    expect_invalid_arg(
        &docker_manifest("  tag = \"app:latest\"\n  dockerfile = \"../../etc/passwd\"\n"),
        "contains `..`",
    );
}

#[test]
fn test_docker_rejects_context_traversal() {
    expect_invalid_arg(
        &docker_manifest("  tag = \"app:latest\"\n  context = \"a/../../b\"\n"),
        "contains `..`",
    );
}

#[test]
fn test_docker_rejects_absolute_context() {
    expect_invalid_arg(
        &docker_manifest("  tag = \"app:latest\"\n  context = \"/etc\"\n"),
        "is absolute",
    );
}

#[test]
fn test_docker_rejects_dockerfile_with_leading_dash() {
    expect_invalid_arg(
        &docker_manifest("  tag = \"app:latest\"\n  dockerfile = \"-f\"\n"),
        "starts with `-`",
    );
}

#[test]
fn test_docker_rejects_dockerfile_with_nul() {
    expect_invalid_arg(
        &docker_manifest("  tag = \"app:latest\"\n  dockerfile = \"Docker\\u0000file\"\n"),
        "control character",
    );
}

#[test]
fn test_docker_requires_node_path() {
    let toml = "
[orchestrator]
schema_version = \"0.1\"
name = \"test\"

[[node]]
id = \"app\"
git = \"https://example.com/app.git\"

  [[node.action]]
  kind = \"docker\"
  tag = \"app:latest\"
";
    expect_invalid_arg(toml, "requires `path`");
}

// ---------------------------------------------------------------------------
// repolith action (federation) — parse + lexical containment (issue #63)
// ---------------------------------------------------------------------------

fn federation_manifest(action_body: &str) -> String {
    format!("{DOCKER_NODE_HEADER}\n  [[node.action]]\n  kind = \"repolith\"\n{action_body}")
}

#[test]
fn test_repolith_parses_with_default_manifest() {
    let m = Manifest::from_toml(&federation_manifest("")).expect("bare repolith action must parse");
    assert_eq!(m.action_ids()[0].0, "app::repolith::0");
    match &m.nodes[0].actions[0] {
        ActionEntry::Repolith { manifest } => assert!(manifest.is_none()),
        other => panic!("expected Repolith, got {other:?}"),
    }
}

#[test]
fn test_repolith_parses_with_explicit_manifest() {
    let m = Manifest::from_toml(&federation_manifest(
        "  manifest = \"stacks/repolith.toml\"\n",
    ))
    .expect("explicit manifest path must parse");
    match &m.nodes[0].actions[0] {
        ActionEntry::Repolith { manifest } => {
            assert_eq!(
                manifest.as_deref().unwrap().to_str(),
                Some("stacks/repolith.toml")
            );
        }
        other => panic!("expected Repolith, got {other:?}"),
    }
}

#[test]
fn test_repolith_rejects_manifest_traversal() {
    expect_invalid_arg(
        &federation_manifest("  manifest = \"../other/repolith.toml\"\n"),
        "contains `..`",
    );
}

#[test]
fn test_repolith_rejects_absolute_manifest() {
    expect_invalid_arg(
        &federation_manifest("  manifest = \"/etc/repolith.toml\"\n"),
        "is absolute",
    );
}

#[test]
fn test_repolith_rejects_manifest_with_leading_dash() {
    expect_invalid_arg(
        &federation_manifest("  manifest = \"-f\"\n"),
        "starts with `-`",
    );
}

#[test]
fn test_repolith_requires_node_path() {
    let toml = "
[orchestrator]
schema_version = \"0.1\"
name = \"test\"

[[node]]
id = \"app\"
git = \"https://example.com/app.git\"

  [[node.action]]
  kind = \"repolith\"
";
    expect_invalid_arg(toml, "requires `path`");
}
