//! A capturing HTTP proxy between the `sbx` CLI and `sandboxd`.
//!
//! The proxy listens on its own Unix socket and forwards every request to the
//! daemon socket unchanged, streaming bodies in both directions. Bodies of the
//! requests matching the capture rules are buffered and recorded before being
//! forwarded. Connection upgrades are not supported (sandbox creation does not
//! use them).

use std::convert::Infallible;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

type ProxyBody = BoxBody<Bytes, hyper::Error>;
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A request body recorded by the proxy.
#[derive(Debug, Clone)]
pub(crate) struct CapturedRequest {
    pub(crate) path: String,
    pub(crate) body: Bytes,
}

/// Requests to capture: `(method, exact path)`.
pub(crate) type CaptureRules = &'static [(&'static str, &'static str)];

/// A running proxy. Stops accepting connections when finished or dropped;
/// connections already accepted are served to their end.
#[derive(Debug)]
pub(crate) struct CaptureProxy {
    socket: PathBuf,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    /// Ends the accept loop when dropped, with the proxy.
    _stop: oneshot::Sender<()>,
}

#[derive(Debug)]
struct Shared {
    upstream: PathBuf,
    rules: CaptureRules,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl CaptureProxy {
    /// Starts listening on `listen`, forwarding to the daemon socket `upstream`.
    pub(crate) fn start(
        listen: PathBuf,
        upstream: PathBuf,
        rules: CaptureRules,
    ) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(&listen);
        let listener = UnixListener::bind(&listen)?;
        std::fs::set_permissions(&listen, std::fs::Permissions::from_mode(0o600))?;

        let captured = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::new(Shared {
            upstream,
            rules,
            captured: Arc::clone(&captured),
        });
        let (stop, mut stopped) = oneshot::channel::<()>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        // A listener that fails stops accepting for good.
                        let Ok((stream, _)) = accepted else { break };
                        tokio::spawn(serve(stream, Arc::clone(&shared)));
                    }
                    _ = &mut stopped => break,
                }
            }
        });
        Ok(Self {
            socket: listen,
            captured,
            _stop: stop,
        })
    }

    /// Path of the socket clients should connect to.
    pub(crate) fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    /// Stops the proxy and returns the captured requests, in arrival order.
    pub(crate) fn finish(self) -> Vec<CapturedRequest> {
        let _ = std::fs::remove_file(&self.socket);
        std::mem::take(
            &mut *self
                .captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// Serves one connection of the CLI, request after request.
async fn serve(stream: UnixStream, shared: Arc<Shared>) {
    let service = service_fn(move |request| forward(request, Arc::clone(&shared)));
    if let Err(error) = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        tracing::trace!(%error, "capture proxy connection ended with an error");
    }
}

/// Forwards one request to the daemon, answering `502` when it cannot be
/// reached.
async fn forward(
    request: Request<Incoming>,
    shared: Arc<Shared>,
) -> Result<Response<ProxyBody>, Infallible> {
    match try_forward(request, &shared).await {
        Ok(response) => Ok(response),
        Err(error) => {
            tracing::warn!(%error, "capture proxy could not reach sandboxd");
            let body = Full::new(Bytes::from(format!(
                "{{\"message\":\"capture proxy: {error}\"}}"
            )))
            .map_err(|never| match never {})
            .boxed();
            let mut response = Response::new(body);
            *response.status_mut() = StatusCode::BAD_GATEWAY;
            Ok(response)
        }
    }
}

/// Forwards one request to the daemon, keeping a copy of it when a capture
/// rule matches.
async fn try_forward(
    request: Request<Incoming>,
    shared: &Shared,
) -> Result<Response<ProxyBody>, BoxError> {
    let (parts, body) = request.into_parts();
    let capture = shared
        .rules
        .iter()
        .any(|(method, path)| parts.method.as_str() == *method && parts.uri.path() == *path);

    let body: ProxyBody = if capture {
        let bytes = body.collect().await?.to_bytes();
        shared
            .captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(CapturedRequest {
                path: parts.uri.path().to_owned(),
                body: bytes.clone(),
            });
        Full::new(bytes).map_err(|never| match never {}).boxed()
    } else {
        body.boxed()
    };

    let stream = UnixStream::connect(&shared.upstream).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake::<_, ProxyBody>(TokioIo::new(stream)).await?;
    // The driver hands its errors to the request or the response body
    // waiting on it, which report them; its own result adds nothing.
    tokio::spawn(connection);
    let response = sender
        .send_request(Request::from_parts(parts, body))
        .await?;
    Ok(response.map(BodyExt::boxed))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Writes the raw HTTP request `raw` to the proxy listening on `socket`
    /// and returns everything it answers.
    async fn send(socket: &Path, raw: &[u8]) -> String {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        stream.write_all(raw).await.unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        answer
    }

    #[tokio::test]
    async fn an_unreachable_daemon_is_a_bad_gateway() {
        let dir = tempfile::tempdir().unwrap();
        let proxy = CaptureProxy::start(
            dir.path().join("proxy.sock"),
            dir.path().join("absent.sock"),
            &[("POST", "/sandbox")],
        )
        .unwrap();

        let answer = send(
            proxy.socket(),
            b"POST /sandbox HTTP/1.1\r\nhost: sandboxd\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 502"), "{answer}");
        assert!(answer.contains("capture proxy"), "{answer}");
        let captured = proxy.finish();
        assert_eq!(captured.len(), 1, "captured before being forwarded");
        assert_eq!(captured[0].body.as_ref(), b"{}");
    }

    #[tokio::test]
    async fn a_daemon_answering_nonsense_is_a_bad_gateway() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = UnixListener::bind(dir.path().join("daemon.sock")).unwrap();
        let hang_up = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut head = [0_u8; 16];
            let _ = stream.read(&mut head).await;
            stream.write_all(b"NOT HTTP\r\n\r\n").await.unwrap();
        });
        let proxy = CaptureProxy::start(
            dir.path().join("proxy.sock"),
            dir.path().join("daemon.sock"),
            &[],
        )
        .unwrap();

        let answer = send(
            proxy.socket(),
            b"GET /daemon/health HTTP/1.1\r\nhost: sandboxd\r\nconnection: close\r\n\r\n",
        )
        .await;
        hang_up.await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 502"), "{answer}");
    }

    #[tokio::test]
    async fn malformed_requests_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let proxy = CaptureProxy::start(
            dir.path().join("proxy.sock"),
            dir.path().join("absent.sock"),
            &[],
        )
        .unwrap();

        let answer = send(proxy.socket(), b"NOT HTTP\r\n\r\n").await;
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
        assert!(proxy.finish().is_empty());
    }
}
