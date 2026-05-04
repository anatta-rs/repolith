//! Fundamental data types for the repolith orchestration engine.
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;
use std::path::PathBuf;
use std::fmt::Display;

/// A unique identifier for an action in the graph.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ActionId(pub String);

impl Display for ActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A 256-bit hash, represent the input or output state of an action.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Sha256(pub [u8; 32]);

impl Display for Sha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

/// The execution context passed to each action.
#[derive(Clone, Debug)]
pub struct Ctx {
    /// Token used to signal cancellation to running tasks.
    pub cancel: CancellationToken,
    /// The working directory for the action.
    pub workdir: PathBuf,
    /// Environment variables to be passed to the action.
    pub env: HashMap<String, String>,
}

/// Determines how the orchestration engine handles failures.
#[derive(Copy, Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
pub enum ExecMode { 
    /// Stop execution immediately upon the first failure.
    FailFast, 
    /// Continue executing independent actions even if some fail.
    KeepGoing
}

/// Errors that can occur during the execution of an action.
#[derive(thiserror::Error, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum BuildError {
    /// A required upstream dependency failed or could not be built.
    #[error("upstream unreachable: {0}")]
    UpstreamUnreachable(String),
    /// The command executed by the action returned a non-zero exit code.
    #[error("Command failed: {exit_code}: {stderr}")]
    CommandFailed{ 
        /// The exit code returned by the command.
        exit_code: i32, 
        /// The standard error output of the command.
        stderr: String 
    },
    /// An I/O error occurred (e.g., file not found, permission denied).
    #[error("IO error: {0}")]
    Io(String), 
    /// The build was cancelled by the user or the system.
    #[error("Build cancelled")]
    Cancelled,
} 

/// The result of a successfully executed action.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BuildOutput {
    /// The hash of the output produced by the action.
    pub output_hash: Sha256,
    /// The standard output captured during execution.
    pub stdout: String,
}

/// Represents an event that occurred during the build process.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum BuildEvent {
    /// The action succeeded.
    Success{
        /// The ID of the action.
        id: ActionId,
        /// The input hash or identifier.
        input: Sha256,
        /// The output hash or identifier.
        output: Sha256,
        /// The duration of the action in milliseconds.
        ms: u64,
    },
    /// The action failed.
    Failed {
        /// The ID of the action.
        id: ActionId,
        /// The input hash or identifier.
        input: Sha256,
        /// The error that caused the failure.
        error: BuildError,
        /// The duration of the action before failure in milliseconds.
        ms: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_action_id_display() {
        let id = ActionId("test-action".to_string());
        assert_eq!(id.to_string(), "test-action");
    }

    #[test]
    fn test_sha256_display() {
        let hash = Sha256([0xab; 32]);
        let expected = "ab".repeat(32);
        assert_eq!(hash.to_string(), expected);
    }

    #[test]
    fn test_build_error_display() {
        let err1 = BuildError::UpstreamUnreachable("dep-1".to_string());
        assert_eq!(err1.to_string(), "upstream unreachable: dep-1");

        let err2 = BuildError::CommandFailed { exit_code: 1, stderr: "boom".to_string() };
        assert_eq!(err2.to_string(), "Command failed: 1: boom");

        let err3 = BuildError::Io("file not found".to_string());
        assert_eq!(err3.to_string(), "IO error: file not found");

        let err4 = BuildError::Cancelled;
        assert_eq!(err4.to_string(), "Build cancelled");
    }

    #[test]
    fn test_ctx_creation() {
        let cancel = CancellationToken::new();
        let ctx = Ctx {
            cancel: cancel.clone(),
            workdir: PathBuf::from("/tmp"),
            env: HashMap::new(),
        };
        assert_eq!(ctx.workdir.to_str().unwrap(), "/tmp");
        assert!(!ctx.cancel.is_cancelled());
        cancel.cancel();
        assert!(ctx.cancel.is_cancelled());
    }

    #[test]
    fn test_serialization_deserialization() {
        // ActionId
        let id = ActionId("action-1".to_string());
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized: ActionId = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        // Sha256
        let hash = Sha256([0x12; 32]);
        let serialized = serde_json::to_string(&hash).unwrap();
        let deserialized: Sha256 = serde_json::from_str(&serialized).unwrap();
        assert_eq!(hash, deserialized);

        // ExecMode
        let mode = ExecMode::FailFast;
        let serialized = serde_json::to_string(&mode).unwrap();
        let deserialized: ExecMode = serde_json::from_str(&serialized).unwrap();
        assert_eq!(mode, deserialized);

        // BuildError
        let err = BuildError::CommandFailed { exit_code: 42, stderr: "err".to_string() };
        let serialized = serde_json::to_string(&err).unwrap();
        let deserialized: BuildError = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            BuildError::CommandFailed { exit_code, stderr } => {
                assert_eq!(exit_code, 42);
                assert_eq!(stderr, "err");
            }
            _ => panic!("Wrong error variant"),
        }

        // BuildOutput
        let output = BuildOutput {
            output_hash: hash,
            stdout: "success".to_string(),
        };
        let serialized = serde_json::to_string(&output).unwrap();
        let deserialized: BuildOutput = serde_json::from_str(&serialized).unwrap();
        assert_eq!(output.output_hash, deserialized.output_hash);
        assert_eq!(output.stdout, deserialized.stdout);

        // BuildEvent::Success
        let event_success = BuildEvent::Success {
            id: id.clone(),
            input: hash,
            output: hash,
            ms: 100,
        };
        let serialized = serde_json::to_string(&event_success).unwrap();
        let deserialized: BuildEvent = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            BuildEvent::Success { id: d_id, ms, .. } => {
                assert_eq!(d_id, id);
                assert_eq!(ms, 100);
            }
            BuildEvent::Failed { .. } => panic!("Wrong event variant"),
        }

        // BuildEvent::Failed
        let event_failed = BuildEvent::Failed {
            id: id.clone(),
            input: hash,
            error: err,
            ms: 200,
        };
        let serialized = serde_json::to_string(&event_failed).unwrap();
        let deserialized: BuildEvent = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            BuildEvent::Failed { id: d_id, ms, .. } => {
                assert_eq!(d_id, id);
                assert_eq!(ms, 200);
            }
            BuildEvent::Success { .. } => panic!("Wrong event variant"),
        }
    }
}