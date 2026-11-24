//! HTTP/1.1 over the `sandboxd` Unix socket.
//!
//! Every request opens a fresh connection, as the `sbx` CLI does: the daemon
//! closes idle connections and several routes upgrade theirs. Buffered calls
//! read the whole body; streams and upgrades hand the body or the connection
//! over to the caller.
//!
//! This module is private: the handles of the crate are its only callers, and
//! each request names its operation so that errors and logs say what failed.

mod ndjson;
mod request;
mod response;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{CONNECTION, HOST, UPGRADE, USER_AGENT};
use http::{HeaderMap, HeaderValue, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

pub use self::ndjson::NdjsonStream;
pub(crate) use self::ndjson::{LineParser, parse_json_line};
pub(crate) use self::request::{Deadline, NDJSON, OCTET_STREAM, Request, TAR, segment};
pub(crate) use self::response::{Response, api_error};
use crate::error::{Error, Result};

/// `User-Agent` of every request.
const USER_AGENT_VALUE: &str = concat!("docker-sandbox/", env!("CARGO_PKG_VERSION"));

/// An upgraded connection (`101 Switching Protocols`) and the response headers.
#[derive(Debug)]
pub(crate) struct UpgradedConnection {
    /// Headers of the `101` response.
    pub(crate) headers: HeaderMap,
    /// The raw byte stream.
    pub(crate) io: TokioIo<Upgraded>,
}

/// Sends requests to `sandboxd` over its Unix socket.
///
/// Cloning is cheap; long-lived objects (sessions, interactive executions,
/// streams) keep their own clone.
#[derive(Debug, Clone)]
pub(crate) struct Transport {
    /// Path of the daemon socket.
    socket: PathBuf,
    /// Deadline of regular requests.
    timeout: Duration,
    /// Deadline of long operations.
    long_timeout: Duration,
}

impl Transport {
    /// A transport to the daemon listening on `socket`.
    pub(crate) fn new(socket: PathBuf, timeout: Duration, long_timeout: Duration) -> Self {
        Self {
            socket,
            timeout,
            long_timeout,
        }
    }

    /// Path of the daemon socket.
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    /// Duration a request may take.
    fn deadline(&self, request: &Request) -> Duration {
        match request.deadline {
            Deadline::Regular => self.timeout,
            Deadline::Long => self.long_timeout,
            Deadline::Exactly(duration) => duration,
        }
    }

    /// Sends a request and buffers the response, whatever its status.
    ///
    /// The deadline covers the whole exchange, body included.
    pub(crate) async fn send(&self, request: Request) -> Result<Response> {
        let deadline = self.deadline(&request);
        let (operation, method, uri) = (request.operation, request.method.clone(), request.uri());
        let started = Instant::now();

        let response = tokio::time::timeout(deadline, async {
            let response = self.send_head(request, &uri).await?;
            let (parts, body) = response.into_parts();
            let body = collect(operation, body).await?;
            Ok::<_, Error>(Response {
                operation,
                status: parts.status,
                body,
            })
        })
        .await
        .map_err(|_| Error::Timeout {
            operation,
            after: deadline,
        })??;

        tracing::debug!(operation, %method, %uri, status = response.status.as_u16(), elapsed_ms = started.elapsed().as_millis(), "sandboxd request");
        Ok(response)
    }

    /// Sends a request, fails on error statuses and decodes the JSON body.
    pub(crate) async fn json<T: DeserializeOwned>(&self, request: Request) -> Result<T> {
        self.send(request).await?.error_for_status()?.json()
    }

    /// Sends a request and fails on error statuses, ignoring the body.
    pub(crate) async fn unit(&self, request: Request) -> Result<()> {
        self.send(request).await?.error_for_status().map(drop)
    }

    /// Sends a request, fails on error statuses and returns the raw body.
    pub(crate) async fn bytes(&self, request: Request) -> Result<Bytes> {
        Ok(self.send(request).await?.error_for_status()?.body)
    }

    /// Sends a request and returns once the response head is received.
    ///
    /// The deadline only covers the head: the body may then be read for as
    /// long as the daemon keeps it open. Error statuses are turned into
    /// [`Error::Api`].
    pub(crate) async fn stream(&self, request: Request) -> Result<Incoming> {
        let deadline = self.deadline(&request);
        let (operation, uri) = (request.operation, request.uri());
        let response = tokio::time::timeout(deadline, self.send_head(request, &uri))
            .await
            .map_err(|_| Error::Timeout {
                operation,
                after: deadline,
            })??;
        let (parts, body) = response.into_parts();
        if !parts.status.is_success() {
            let body = collect(operation, body).await?;
            return Err(api_error(operation, parts.status, &body).into());
        }
        tracing::debug!(operation, %uri, status = parts.status.as_u16(), "sandboxd stream opened");
        Ok(body)
    }

    /// Opens an NDJSON stream decoded with `parse`.
    pub(crate) async fn ndjson<T>(
        &self,
        request: Request,
        parse: LineParser<T>,
    ) -> Result<NdjsonStream<T>> {
        let operation = request.operation;
        let body = self.stream(request.accept(NDJSON)).await?;
        Ok(NdjsonStream::new(operation, body, parse))
    }

    /// Sends a request asking to switch to `protocol` (`tcp` for executions,
    /// `ssh` for the SSH tunnel) and returns the upgraded connection.
    ///
    /// Fails with [`Error::Api`] if the daemon answers with an error status,
    /// and with [`Error::InvalidRequest`] if it answers without switching
    /// protocols.
    pub(crate) async fn upgrade(
        &self,
        mut request: Request,
        protocol: &'static str,
    ) -> Result<UpgradedConnection> {
        request
            .headers
            .insert(CONNECTION, HeaderValue::from_static("Upgrade"));
        request
            .headers
            .insert(UPGRADE, HeaderValue::from_static(protocol));
        let deadline = self.deadline(&request);
        let (operation, uri) = (request.operation, request.uri());

        let response = tokio::time::timeout(deadline, self.send_head(request, &uri))
            .await
            .map_err(|_| Error::Timeout {
                operation,
                after: deadline,
            })??;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            let status = response.status();
            let body = collect(operation, response.into_body()).await?;
            return Err(if status.is_success() {
                Error::InvalidRequest(format!(
                    "{operation}: sandboxd answered {status} instead of switching to {protocol}"
                ))
            } else {
                api_error(operation, status, &body).into()
            });
        }
        let headers = response.headers().clone();
        let upgraded = hyper::upgrade::on(response)
            .await
            .map_err(|source| Error::Http { operation, source })?;
        tracing::debug!(operation, %uri, protocol, "sandboxd connection upgraded");
        Ok(UpgradedConnection {
            headers,
            io: TokioIo::new(upgraded),
        })
    }

    /// Opens a connection, sends the request and waits for the response head.
    async fn send_head(&self, request: Request, uri: &str) -> Result<http::Response<Incoming>> {
        let operation = request.operation;
        let http_error = |source| Error::Http { operation, source };
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|source| Error::Connect {
                path: self.socket.clone(),
                source,
            })?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(http_error)?;
        // The driver hands its errors to the request or the response body
        // waiting on it, which report them; its own result adds nothing.
        tokio::spawn(connection.with_upgrades());

        let mut builder = http::Request::builder()
            .method(request.method)
            .uri(uri)
            .header(HOST, "sandboxd")
            .header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(headers) = builder.headers_mut() {
            if !request.headers.contains_key(CONNECTION) {
                headers.insert(CONNECTION, HeaderValue::from_static("close"));
            }
            headers.extend(request.headers);
        }
        let http_request = builder
            .body(Full::new(request.body))
            .map_err(|e| Error::InvalidRequest(format!("{operation}: {e}")))?;
        sender.send_request(http_request).await.map_err(http_error)
    }
}

/// Reads a whole response body.
async fn collect(operation: &'static str, body: Incoming) -> Result<Bytes> {
    Ok(body
        .collect()
        .await
        .map_err(|source| Error::Http { operation, source })?
        .to_bytes())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    use super::*;

    #[tokio::test]
    async fn uris_the_http_library_rejects_are_invalid_requests() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sandboxd.sock");
        let _daemon = UnixListener::bind(&socket).unwrap();
        let transport = Transport::new(socket, Duration::from_secs(5), Duration::from_secs(5));

        // Handles escape every dynamic part; a raw space is never sent.
        let error = transport
            .send(Request::get("sandbox inspection", "/sandbox/job 42"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::InvalidRequest(ref m) if m.starts_with("sandbox inspection: ")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn answers_that_are_not_http_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sandboxd.sock");
        let daemon = UnixListener::bind(&socket).unwrap();
        let answer = tokio::spawn(async move {
            let (mut stream, _) = daemon.accept().await.unwrap();
            let mut head = [0_u8; 16];
            let _ = stream.read(&mut head).await;
            stream.write_all(b"NOT HTTP\r\n\r\n").await.unwrap();
        });
        let transport = Transport::new(socket, Duration::from_secs(5), Duration::from_secs(5));

        let error = transport
            .send(Request::get("daemon info", "/daemon/info"))
            .await
            .unwrap_err();
        answer.await.unwrap();
        assert!(
            matches!(
                error,
                Error::Http {
                    operation: "daemon info",
                    ..
                }
            ),
            "{error}"
        );
    }
}
