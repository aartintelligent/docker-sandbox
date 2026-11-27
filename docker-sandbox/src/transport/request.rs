//! Outgoing requests: method, path, query, headers, body and deadline.
//!
//! Handles build a [`Request`] per operation and hand it to the
//! [`Transport`](super::Transport). Dynamic path parts go through [`segment`]
//! and query values are percent-encoded when the URI is built, so that no
//! sandbox, image or setting name can alter the route.

use std::fmt;
use std::io;
use std::time::Duration;

use bytes::Bytes;
use http::header::{ACCEPT, CONTENT_TYPE};
use http::{HeaderMap, HeaderValue, Method};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Serialize;
use zeroize::Zeroizing;

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

/// An outgoing request. Its [`Debug`](fmt::Debug) shows the size of the body,
/// never its content, which may carry secrets.
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

    /// Sets a JSON body that carries secret values: written into one buffer
    /// sized beforehand, so that its growth leaves no partial copy behind, and
    /// wiped from memory once the request is done with it.
    pub(crate) fn secret_json<T: Serialize + ?Sized>(self, body: &T) -> Result<Self> {
        let operation = self.operation;
        let error = |e: serde_json::Error| {
            Error::InvalidRequest(format!("{operation}: cannot serialize the body: {e}"))
        };
        let mut size = ByteCount(0);
        serde_json::to_writer(&mut size, body).map_err(error)?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(size.0));
        serde_json::to_writer(&mut *bytes, body).map_err(error)?;
        Ok(self.body(Bytes::from_owner(bytes), JSON))
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

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("operation", &self.operation)
            .field("method", &self.method)
            .field("path", &self.path)
            .field("query", &self.query)
            .field("body_bytes", &self.body.len())
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

/// A writer that only counts the bytes written to it, to size a buffer before
/// writing into it.
struct ByteCount(usize);

impl io::Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn secret_bodies_are_the_json_of_the_value_without_a_copy_in_the_debug_output() {
        let body = serde_json::json!({ "custom_secrets": [{ "value": "dummy-value" }] });
        let request = Request::post("credentials update", "/sandbox/job-42/credentials")
            .secret_json(&body)
            .unwrap();
        assert_eq!(request.body, serde_json::to_vec(&body).unwrap());
        assert_eq!(request.headers[CONTENT_TYPE], JSON);
        let debug = format!("{request:?}");
        assert!(!debug.contains("dummy-value"), "{debug}");
        assert!(debug.contains("body_bytes"), "{debug}");
    }

    #[test]
    fn byte_counts_size_what_is_written() {
        let mut count = ByteCount(0);
        count.write_all(b"abc").unwrap();
        count.flush().unwrap();
        assert_eq!(count.0, 3);
    }

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
        let error = Request::post("credentials update", "/sandbox/a/credentials")
            .secret_json(&Unserializable)
            .unwrap_err();
        assert!(
            matches!(error, Error::InvalidRequest(ref m) if m == "credentials update: cannot serialize the body: not serializable"),
            "{error}"
        );
    }
}
