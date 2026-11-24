//! Error type of the crate.
//!
//! Every fallible operation returns [`Error`]. Errors the daemon answers with a
//! status code are carried by [`ApiError`], which names the operation that
//! failed; the classification methods ([`Error::is_not_found`],
//! [`Error::is_conflict`], [`Error::is_transient`], …) let callers react
//! without matching on status codes.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use http::StatusCode;

use crate::exec::ExecOutput;

/// Convenient alias for results returned by this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors returned when talking to `sandboxd`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No socket path was given and none can be derived from the environment.
    #[error("no sandboxd socket path given and neither DOCKER_SANDBOXES_API nor HOME is set")]
    NoSocketPath,

    /// The Unix socket could not be reached.
    #[error("cannot connect to sandboxd at {path}: {source}")]
    Connect {
        /// Socket path that was tried.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The HTTP exchange failed after the connection was established.
    #[error("{operation}: HTTP error while talking to sandboxd: {source}")]
    Http {
        /// Operation that was running.
        operation: &'static str,
        /// Underlying HTTP error.
        #[source]
        source: hyper::Error,
    },

    /// The operation did not complete within its deadline.
    #[error("{operation}: no outcome within {after:?}")]
    Timeout {
        /// Operation that was running.
        operation: &'static str,
        /// Deadline that expired.
        after: Duration,
    },

    /// `sandboxd` answered with an error status.
    #[error(transparent)]
    Api(#[from] ApiError),

    /// `sandboxd` reported a failure inside a progress stream.
    ///
    /// Sandbox creation and image pulls answer HTTP 200 and report failures
    /// in-band, so this error does not carry a status code.
    #[error("{operation} failed: {message}")]
    Failed {
        /// Operation that failed.
        operation: &'static str,
        /// Failure reported by the daemon.
        message: String,
    },

    /// A response body did not have the expected shape.
    #[error("{operation}: unexpected response body: {source}")]
    Decode {
        /// Operation whose response failed to decode.
        operation: &'static str,
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },

    /// The daemon speaks an API version this crate was not validated against.
    #[error("unsupported sandboxd API version {found} (supported: {supported})")]
    UnsupportedVersion {
        /// Version reported by the daemon.
        found: String,
        /// Range supported by this crate.
        supported: String,
    },

    /// The request could not be built from the given arguments.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// An upgraded connection (interactive execution, SSH tunnel) failed.
    #[error("interactive connection failed: {0}")]
    Connection(#[source] std::io::Error),

    /// A host file or directory could not be read or written.
    #[error("cannot access {path}: {source}")]
    File {
        /// Host path involved.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A tar archive exchanged with the daemon could not be built or read.
    #[error("{operation}: {message}")]
    Archive {
        /// Operation that handled the archive.
        operation: &'static str,
        /// What is wrong with it.
        message: String,
    },

    /// A command exited with a non-zero code, reported by
    /// [`ExecOutput::check`]. The outputs are kept for inspection.
    #[error("{}", describe_failure(.0))]
    CommandFailed(Box<ExecOutput>),

    /// A kit package could not be read or written.
    #[error("invalid kit package at {path}: {message}")]
    Package {
        /// Package directory or file.
        path: PathBuf,
        /// What is wrong with it.
        message: String,
    },

    /// The `sbx` CLI could not compile a kit composition (feature
    /// `kit-compiler`): it could not be started, rejected the composition, or
    /// sent no creation request.
    #[error("kit compilation failed: {0}")]
    Compile(String),
}

impl Error {
    /// The daemon error, if this is an [`Error::Api`].
    #[must_use]
    pub fn api(&self) -> Option<&ApiError> {
        match self {
            Self::Api(error) => Some(error),
            _ => None,
        }
    }

    /// HTTP status returned by the daemon, if this is an [`Error::Api`].
    #[must_use]
    pub fn status(&self) -> Option<StatusCode> {
        self.api().map(ApiError::status)
    }

    /// Machine-readable code of the daemon error, such as `invalid_input` or
    /// `exec_not_running`, when the daemon provides one.
    #[must_use]
    pub fn api_code(&self) -> Option<&str> {
        self.api().and_then(ApiError::code)
    }

    /// Whether the daemon reported that the target does not exist.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.status() == Some(StatusCode::NOT_FOUND)
    }

    /// Whether the daemon reported a state conflict, such as an operation on a
    /// stopped sandbox or an operation already in progress.
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        self.status() == Some(StatusCode::CONFLICT)
    }

    /// Whether the daemon rejected the request as malformed or invalid.
    #[must_use]
    pub fn is_invalid_input(&self) -> bool {
        self.status() == Some(StatusCode::BAD_REQUEST)
    }

    /// Whether the operation ran out of time.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout { .. })
    }

    /// Whether retrying the same request later may succeed: the daemon was
    /// unreachable, slow, busy or failing.
    ///
    /// The daemon also answers a few invalid inputs with a server error: an
    /// image archive it cannot load, an upload into a directory that does not
    /// exist. Retrying those does not help.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Connect { .. } | Self::Timeout { .. } | Self::Connection(_) => true,
            Self::Api(error) => {
                error.status().is_server_error() || error.status() == StatusCode::CONFLICT
            }
            _ => false,
        }
    }
}

/// An error status answered by `sandboxd`.
///
/// The daemon answers errors with `{"message": …}`, sometimes with a
/// machine-readable `code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// Operation whose request was rejected.
    operation: &'static str,
    /// HTTP status of the response.
    status: StatusCode,
    /// Machine-readable code, when the daemon provides one.
    code: Option<String>,
    /// Human-readable message.
    message: String,
}

impl ApiError {
    /// An error answered with `status` to the request of `operation`.
    pub(crate) fn new(
        operation: &'static str,
        status: StatusCode,
        code: Option<String>,
        message: String,
    ) -> Self {
        Self {
            operation,
            status,
            code,
            message,
        }
    }

    /// Operation whose request was rejected, e.g. `sandbox start`.
    #[must_use]
    pub fn operation(&self) -> &'static str {
        self.operation
    }

    /// HTTP status of the response.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Machine-readable code, such as `invalid_input`, `exec_not_running` or
    /// `desired_port_not_found`.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// Human-readable message of the daemon.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: sandboxd returned {}: {}",
            self.operation, self.status, self.message
        )
    }
}

impl std::error::Error for ApiError {}

/// `command exited with code N`, followed by the end of its standard error.
fn describe_failure(output: &ExecOutput) -> String {
    let mut message = format!("command exited with code {}", output.exit_code);
    let lines: Vec<&str> = output.stderr.trim().lines().collect();
    if !lines.is_empty() {
        message.push_str(": ");
        message.push_str(&lines[lines.len().saturating_sub(5)..].join(" / "));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon error answered with `status` to a start request.
    fn api(status: StatusCode) -> Error {
        Error::Api(ApiError::new(
            "sandbox start",
            status,
            Some("not_running".to_owned()),
            "sandbox \"x\" is not running".to_owned(),
        ))
    }

    #[test]
    fn api_errors_are_classified_and_name_their_operation() {
        let conflict = api(StatusCode::CONFLICT);
        assert!(conflict.is_conflict() && conflict.is_transient());
        assert_eq!(conflict.api_code(), Some("not_running"));
        assert_eq!(
            conflict.to_string(),
            "sandbox start: sandboxd returned 409 Conflict: sandbox \"x\" is not running"
        );
        assert!(api(StatusCode::NOT_FOUND).is_not_found());
        assert!(api(StatusCode::BAD_REQUEST).is_invalid_input());
        assert!(!api(StatusCode::BAD_REQUEST).is_transient());
    }

    #[test]
    fn command_failures_quote_the_end_of_stderr() {
        let failure = Error::CommandFailed(Box::new(ExecOutput::new(2, "", "a\nb\nc\nd\ne\nf\n")));
        assert_eq!(
            failure.to_string(),
            "command exited with code 2: b / c / d / e / f"
        );
        let silent = Error::CommandFailed(Box::new(ExecOutput::new(1, "out", "")));
        assert_eq!(silent.to_string(), "command exited with code 1");
    }

    #[test]
    fn other_errors_carry_no_daemon_error_and_are_not_retried() {
        let failed = Error::Failed {
            operation: "image pull",
            message: "manifest unknown".to_owned(),
        };
        assert!(failed.api().is_none());
        assert!(!failed.is_transient());
    }
}
