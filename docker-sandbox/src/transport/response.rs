//! Buffered responses and the decoding of daemon errors.
//!
//! The daemon answers errors with `{"message": …}`, sometimes with a `code` or
//! an `error` field instead, and some routes answer plain text. [`api_error`]
//! turns any of those into an [`ApiError`] naming the failed operation.

use bytes::Bytes;
use http::StatusCode;
use serde::de::DeserializeOwned;

use crate::error::{ApiError, Error, Result};

/// A buffered response.
#[derive(Debug)]
pub(crate) struct Response {
    /// Operation the response answers.
    pub(crate) operation: &'static str,
    /// HTTP status.
    pub(crate) status: StatusCode,
    /// Whole body.
    pub(crate) body: Bytes,
}

impl Response {
    /// Turns a non-success status into [`Error::Api`].
    pub(crate) fn error_for_status(self) -> Result<Self> {
        if self.status.is_success() {
            return Ok(self);
        }
        Err(api_error(self.operation, self.status, &self.body).into())
    }

    /// Decodes the body as JSON.
    pub(crate) fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(|source| Error::Decode {
            operation: self.operation,
            source,
        })
    }
}

/// Builds the [`ApiError`] of an error response to `operation`.
pub(crate) fn api_error(operation: &'static str, status: StatusCode, body: &[u8]) -> ApiError {
    /// Fields the daemon uses in error bodies.
    #[derive(serde::Deserialize)]
    struct ErrorBody {
        /// Human-readable message, the usual field.
        message: Option<String>,
        /// Human-readable message, used by a few routes.
        error: Option<String>,
        /// Machine-readable code.
        code: Option<String>,
    }

    let parsed: Option<ErrorBody> = serde_json::from_slice(body).ok();
    let (message, code) = match parsed {
        Some(b) => (b.message.or(b.error), b.code),
        None => (None, None),
    };
    let message = message
        .or_else(|| {
            let text = String::from_utf8_lossy(body).trim().to_owned();
            (!text.is_empty()).then_some(text)
        })
        .unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("unknown error")
                .to_owned()
        });
    ApiError::new(operation, status, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_errors_prefer_message_then_error_then_text() {
        let err = api_error(
            "exec resize",
            StatusCode::CONFLICT,
            br#"{"code":"exec_not_running","message":"gone"}"#,
        );
        assert_eq!(err.code(), Some("exec_not_running"));
        assert_eq!(err.message(), "gone");
        assert_eq!(err.operation(), "exec resize");

        let err = api_error("x", StatusCode::CONFLICT, br#"{"error":"stopped"}"#);
        assert_eq!(err.message(), "stopped");

        let err = api_error("x", StatusCode::NOT_FOUND, b"404 page not found");
        assert_eq!(err.message(), "404 page not found");

        let err = api_error("x", StatusCode::BAD_GATEWAY, b"");
        assert_eq!(err.message(), "Bad Gateway");
    }
}
