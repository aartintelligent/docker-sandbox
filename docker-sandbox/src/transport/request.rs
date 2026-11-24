//! Outgoing requests: method, path, query, headers, body and deadline.
//!
//! Handles build a [`Request`] per operation and hand it to the
//! [`Transport`](super::Transport). Dynamic path parts go through [`segment`]
//! and query values are percent-encoded when the URI is built, so that no
//! sandbox, image or setting name can alter the route.

use std::time::Duration;

use bytes::Bytes;
use http::header::{ACCEPT, CONTENT_TYPE};
use http::{HeaderMap, HeaderValue, Method};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Serialize;

use crate::error::{Error, Result};

/// Characters left unescaped in path segments and query components (RFC 3986
/// unreserved set).
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Media type of JSON bodies.
pub(crate) const JSON: &str = "application/json";
/// Media type of streamed progress responses.
pub(crate) const NDJSON: &str = "application/x-ndjson";
/// Media type of tar archives.
pub(crate) const TAR: &str = "application/x-tar";
/// Media type of opaque binary bodies.
pub(crate) const OCTET_STREAM: &str = "application/octet-stream";

/// Percent-encodes a value for use as a single path segment.
pub(crate) fn segment(value: &str) -> String {
    utf8_percent_encode(value, UNRESERVED).to_string()
}

/// How long the transport waits for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Deadline {
    /// The regular timeout of the client.
    Regular,
    /// The long-operation timeout of the client: creation, start, stop,
    /// execution, file and image transfers.
    Long,
    /// An explicit duration chosen by the caller.
    Exactly(Duration),
}

/// An outgoing request.
#[derive(Debug)]
pub(crate) struct Request {
    /// Name of the operation, used in errors and logs (`sandbox start`).
    pub(crate) operation: &'static str,
    /// HTTP method.
    pub(crate) method: Method,
    /// Encoded path, without the query string.
    pub(crate) path: String,
    /// Query parameters, encoded when the URI is built.
    query: Vec<(&'static str, String)>,
    /// Request headers.
    pub(crate) headers: HeaderMap,
    /// Request body, empty by default.
    pub(crate) body: Bytes,
    /// Deadline of the exchange.
    pub(crate) deadline: Deadline,
}

impl Request {
    /// Starts a request of `operation`. `path` must already be encoded; use
    /// [`segment`] for its dynamic parts.
    pub(crate) fn new(operation: &'static str, method: Method, path: impl Into<String>) -> Self {
        Self {
            operation,
            method,
            path: path.into(),
            query: Vec::new(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            deadline: Deadline::Regular,
        }
    }

    /// A `GET` request.
    pub(crate) fn get(operation: &'static str, path: impl Into<String>) -> Self {
        Self::new(operation, Method::GET, path)
    }

    /// A `POST` request.
    pub(crate) fn post(operation: &'static str, path: impl Into<String>) -> Self {
        Self::new(operation, Method::POST, path)
    }

    /// A `PUT` request.
    pub(crate) fn put(operation: &'static str, path: impl Into<String>) -> Self {
        Self::new(operation, Method::PUT, path)
    }

    /// A `DELETE` request.
    pub(crate) fn delete(operation: &'static str, path: impl Into<String>) -> Self {
        Self::new(operation, Method::DELETE, path)
    }

    /// Appends a query parameter. The value is percent-encoded.
    pub(crate) fn query(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.query.push((key, value.into()));
        self
    }

    /// Sets a JSON body.
    pub(crate) fn json<T: Serialize + ?Sized>(self, body: &T) -> Result<Self> {
        let bytes = serde_json::to_vec(body).map_err(|e| {
            Error::InvalidRequest(format!(
                "{}: cannot serialize the body: {e}",
                self.operation
            ))
        })?;
        Ok(self.body(Bytes::from(bytes), JSON))
    }

    /// Sets an empty JSON object as body, as the CLI sends to action routes.
    pub(crate) fn empty_json(self) -> Self {
        self.body(Bytes::from_static(b"{}"), JSON)
    }

    /// Sets a raw body with its media type.
    pub(crate) fn body(mut self, body: Bytes, content_type: &'static str) -> Self {
        self.headers
            .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        self.body = body;
        self
    }

    /// Sets the `Accept` header.
    pub(crate) fn accept(mut self, media_type: &'static str) -> Self {
        self.headers
            .insert(ACCEPT, HeaderValue::from_static(media_type));
        self
    }

    /// Uses the long-operation timeout of the client.
    pub(crate) fn long_running(mut self) -> Self {
        self.deadline = Deadline::Long;
        self
    }

    /// Uses `timeout` when given, the long-operation timeout otherwise.
    pub(crate) fn long_running_or(mut self, timeout: Option<Duration>) -> Self {
        self.deadline = timeout.map_or(Deadline::Long, Deadline::Exactly);
        self
    }

    /// Path and encoded query string.
    pub(crate) fn uri(&self) -> String {
        if self.query.is_empty() {
            return self.path.clone();
        }
        let query: Vec<String> = self
            .query
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    utf8_percent_encode(k, UNRESERVED),
                    utf8_percent_encode(v, UNRESERVED)
                )
            })
            .collect();
        format!("{}?{}", self.path, query.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_and_queries_are_escaped() {
        assert_eq!(segment("job-42"), "job-42");
        assert_eq!(segment("a/b c"), "a%2Fb%20c");

        let request = Request::get("image inspect", "/docker/images/inspect")
            .query("name", "docker.io/sbx/shell@sha256:ab");
        assert_eq!(
            request.uri(),
            "/docker/images/inspect?name=docker.io%2Fsbx%2Fshell%40sha256%3Aab"
        );
    }

    #[test]
    fn deadlines_default_to_the_regular_timeout() {
        let request = Request::post("exec", "/sandbox/a/exec");
        assert_eq!(request.deadline, Deadline::Regular);
        assert_eq!(request.long_running_or(None).deadline, Deadline::Long);
        let explicit =
            Request::post("exec", "/sandbox/a/exec").long_running_or(Some(Duration::from_secs(3)));
        assert_eq!(explicit.deadline, Deadline::Exactly(Duration::from_secs(3)));
    }

    /// A body that cannot be serialized.
    struct Unserializable;

    impl Serialize for Unserializable {
        fn serialize<S: serde::Serializer>(&self, _: S) -> std::result::Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("not serializable"))
        }
    }

    #[test]
    fn bodies_that_cannot_be_serialized_are_invalid_requests() {
        let error = Request::post("sandbox save", "/sandbox/a/save")
            .json(&Unserializable)
            .unwrap_err();
        assert!(
            matches!(error, Error::InvalidRequest(ref m) if m == "sandbox save: cannot serialize the body: not serializable"),
            "{error}"
        );
    }
}
