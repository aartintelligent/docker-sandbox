//! Outputs of executions: synchronous results, interactive chunks and states.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::Extra;
use crate::error::{Error, Result};

/// Result of a synchronous execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutput {
    /// Exit code of the process.
    pub exit_code: i32,
    /// Standard output.
    #[serde(default)]
    pub stdout: String,
    /// Standard error.
    #[serde(default)]
    pub stderr: String,
}

impl ExecOutput {
    /// An output with the given exit code and streams.
    pub fn new(exit_code: i32, stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self {
            exit_code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// Whether the process exited with code 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }

    /// Returns the output when the process exited with code 0.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CommandFailed`], carrying the output, for any other
    /// exit code.
    pub fn check(self) -> Result<Self> {
        if self.success() {
            Ok(self)
        } else {
            Err(Error::CommandFailed(Box::new(self)))
        }
    }
}

/// State of an execution, from `GET /sandbox/{name}/exec/{exec_id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecInspect {
    /// Execution identifier.
    pub id: String,
    /// Whether the process is still running.
    pub running: bool,
    /// Exit code, once the process has exited.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Output stream a chunk belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputStream {
    /// Standard output, or the merged terminal output when a TTY is allocated.
    Stdout,
    /// Standard error, including errors reported by the daemon itself.
    Stderr,
}

/// A chunk of process output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChunk {
    /// Stream the chunk belongs to.
    pub stream: OutputStream,
    /// Raw bytes.
    pub data: Bytes,
}

/// Collected result of an interactive execution, returned by
/// [`ExecSession::collect`](super::ExecSession::collect).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachOutput {
    /// Standard output (merged terminal output with a TTY).
    pub stdout: Vec<u8>,
    /// Standard error.
    pub stderr: Vec<u8>,
    /// Exit code of the process.
    pub exit_code: i32,
}

impl AttachOutput {
    /// Whether the process exited with code 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}
