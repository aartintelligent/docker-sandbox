//! Minimal HTTP/1.1 client over a Unix socket (hyper 1 on tokio).
//!
//! Every request opens a fresh connection: the goal is to observe the daemon, not to be
//! efficient. Connections are driven with upgrade support, so the same client can switch to
//! a raw stream for Docker-style hijacked endpoints (exec, attach, SSH).

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use std::path::PathBuf;
use std::str::FromStr;
use tokio::net::UnixStream;

/// HTTP client bound to the Unix socket of the daemon.
#[derive(Debug, Clone)]
pub(crate) struct SockClient {
    /// Path of the Unix socket to connect to.
    pub path: PathBuf,
}

/// Response with its body fully read into memory.
pub(crate) struct RawResponse {
    /// HTTP status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Complete body, or a placeholder when [`SockClient::request`] gave up waiting for it.
    pub body: Bytes,
}

impl SockClient {
    /// Creates a client for the socket at `path`. No connection is opened until a request is
    /// sent.
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Opens a new connection, performs the HTTP/1 handshake and returns the request sender.
    ///
    /// The connection is driven by a background task with upgrades enabled.
    async fn sender(&self) -> Result<hyper::client::conn::http1::SendRequest<Full<Bytes>>> {
        let stream = UnixStream::connect(&self.path)
            .await
            .with_context(|| format!("cannot connect to {}", self.path.display()))?;
        let (sender, conn) =
            hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        Ok(sender)
    }

    /// Builds a request for `method` (case-insensitive) and `path`.
    ///
    /// `Content-Length`, `Transfer-Encoding` and `Connection` headers are dropped because hyper
    /// sets them itself. `Host: sandboxd` is added when no `Host` header is given, and
    /// `Content-Type: application/json` is added when there is a body without a content type.
    pub(crate) fn build(
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Option<Bytes>,
    ) -> Result<Request<Full<Bytes>>> {
        let method = Method::from_str(&method.to_uppercase())?;
        let mut req = Request::builder().method(method).uri(path);
        let mut has_ct = false;
        let mut has_host = false;
        for (k, v) in headers {
            let lk = k.to_ascii_lowercase();
            if lk == "content-length" || lk == "transfer-encoding" || lk == "connection" {
                continue; // Set by hyper.
            }
            has_ct |= lk == "content-type";
            has_host |= lk == "host";
            req = req.header(HeaderName::from_str(k)?, HeaderValue::from_str(v)?);
        }
        if !has_host {
            req = req.header("host", "sandboxd");
        }
        if body.is_some() && !has_ct {
            req = req.header("content-type", "application/json");
        }
        Ok(req.body(Full::new(body.unwrap_or_default()))?)
    }

    /// Sends `req` on a new connection and reads the whole response body, without any time
    /// limit.
    pub(crate) async fn send(&self, req: Request<Full<Bytes>>) -> Result<RawResponse> {
        let mut s = self.sender().await?;
        let res = s.send_request(req).await?;
        let (parts, body) = res.into_parts();
        let body = body.collect().await?.to_bytes();
        Ok(RawResponse {
            status: parts.status,
            headers: parts.headers,
            body,
        })
    }

    /// Sends a bodiless probe request with time limits, so that a streaming route (session,
    /// logs, attach...) cannot stall the probe.
    ///
    /// Connecting and receiving the response headers are each limited to 5 seconds, and fail
    /// past that. Reading the body is limited to 3 seconds: if the body is still incomplete by
    /// then, the status and headers are kept and the body is replaced with a placeholder that
    /// says so.
    pub(crate) async fn request(&self, method: &str, path: &str) -> Result<RawResponse> {
        use std::time::Duration;
        use tokio::time::timeout;
        let req = Self::build(method, path, &[], None)?;
        let mut s = timeout(Duration::from_secs(5), self.sender())
            .await
            .context("connection timed out (5 s)")??;
        let res = timeout(Duration::from_secs(5), s.send_request(req))
            .await
            .context("timed out waiting for response headers (5 s)")??;
        let (parts, body) = res.into_parts();
        let body = match timeout(Duration::from_secs(3), body.collect()).await {
            Ok(b) => b?.to_bytes(),
            Err(_) => Bytes::from_static(b"<stream: body incomplete after 3 s, cut off by audit>"),
        };
        Ok(RawResponse {
            status: parts.status,
            headers: parts.headers,
            body,
        })
    }

    /// Sends `req` and reads the body for at most `wait`, keeping what arrived: for
    /// streaming routes, whose body never ends.
    ///
    /// # Errors
    ///
    /// Fails if no response arrives within `wait` or the connection breaks.
    pub(crate) async fn send_for(
        &self,
        req: Request<Full<Bytes>>,
        wait: std::time::Duration,
    ) -> Result<RawResponse> {
        let deadline = tokio::time::Instant::now() + wait;
        let mut s = self.sender().await?;
        let res = tokio::time::timeout_at(deadline, s.send_request(req))
            .await
            .context("no response in time")??;
        let (parts, mut body) = res.into_parts();
        let mut data = Vec::new();
        while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, body.frame()).await {
            if let Ok(chunk) = frame?.into_data() {
                data.extend_from_slice(&chunk);
            }
        }
        Ok(RawResponse {
            status: parts.status,
            headers: parts.headers,
            body: Bytes::from(data),
        })
    }

    /// Sends a bodiless `GET` to a streaming route (such as `/events`) and calls `on_line`
    /// with every complete line of the body, until the daemon ends the stream.
    ///
    /// # Errors
    ///
    /// Fails if the request cannot be sent or the stream breaks.
    pub(crate) async fn stream_lines(
        &self,
        path: &str,
        mut on_line: impl FnMut(&str),
    ) -> Result<()> {
        let req = Self::build("GET", path, &[], None)?;
        let mut s = self.sender().await?;
        let mut body = s.send_request(req).await?.into_body();
        let mut pending: Vec<u8> = Vec::new();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame?.into_data() {
                pending.extend_from_slice(&data);
                while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    let line = String::from_utf8_lossy(&line[..end]);
                    if !line.trim().is_empty() {
                        on_line(line.trim_end());
                    }
                }
            }
        }
        Ok(())
    }

    /// Sends a Docker-style `Connection: Upgrade` request, as used by hijacked exec and attach
    /// endpoints, and returns the raw bidirectional stream once the daemon answers
    /// `101 Switching Protocols`.
    ///
    /// The `Upgrade` header defaults to `tcp` unless `req` already sets one. Any other status
    /// is an error that includes the response body.
    pub(crate) async fn upgrade(
        &self,
        mut req: Request<Full<Bytes>>,
    ) -> Result<TokioIo<hyper::upgrade::Upgraded>> {
        req.headers_mut()
            .insert("connection", HeaderValue::from_static("Upgrade"));
        // `tcp` for exec/attach; `ssh` for /sandbox/:name/ssh (-H 'Upgrade: ssh').
        req.headers_mut()
            .entry("upgrade")
            .or_insert(HeaderValue::from_static("tcp"));
        let mut s = self.sender().await?;
        let res = s.send_request(req).await?;
        let status = res.status();
        if status != StatusCode::SWITCHING_PROTOCOLS {
            let body = res.into_body().collect().await?.to_bytes();
            bail!(
                "connection not upgraded (HTTP {status}): {}",
                String::from_utf8_lossy(&body)
            );
        }
        let upgraded = hyper::upgrade::on(res).await?;
        Ok(TokioIo::new(upgraded))
    }
}

/// Returns at most the first `max` bytes of `body` as lossy UTF-8, followed by a
/// `…(+N bytes)` marker when bytes were left out.
pub(crate) fn preview(body: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(&body[..body.len().min(max)]).to_string();
    if body.len() > max {
        format!("{s}…(+{} bytes)", body.len() - max)
    } else {
        s
    }
}
