//! Test support: a minimal fake `sandboxd` serving canned responses on a Unix
//! socket and recording the requests it receives.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use docker_sandbox::Client;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

/// Connects a client to `daemon`.
pub(crate) async fn connect(daemon: &FakeDaemon) -> Client {
    Client::builder()
        .with_socket(daemon.socket())
        .connect()
        .await
        .expect("connect to fake daemon")
}

/// A sandbox description as the daemon returns it, with an unknown field.
pub(crate) fn sandbox_json(name: &str, status: &str) -> Value {
    serde_json::json!({
        "id": "11765de3-dd25-4a0f-810f-af8f3bccc1ca",
        "name": name,
        "agent": "shell",
        "status": status,
        "workspace": "",
        "labels": { "com.docker.sandbox.agent": "shell" },
        "future_field": { "kept": true }
    })
}

/// Variable that turns a re-executed test binary into a helper process, set to
/// the name of the test to act as.
pub(crate) const HELPER_ENV: &str = "DOCKER_SANDBOX_TEST_HELPER";

/// A command running the test `name` of this test binary as a helper process.
///
/// A test can neither change its own environment safely nor start a program
/// the repository does not build: it re-executes itself instead, as the tests
/// of Go's `os/exec` do. The helper test checks [`is_helper`] and returns at
/// once in a normal run.
pub(crate) fn helper_process(name: &str) -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture"])
        .env(HELPER_ENV, name);
    command
}

/// Whether this process was started by [`helper_process`] to act as `name`.
pub(crate) fn is_helper(name: &str) -> bool {
    std::env::var_os(HELPER_ENV).is_some_and(|value| value == name)
}

/// A tar archive holding one regular file.
pub(crate) fn tar_with_file(name: &str, contents: &[u8]) -> Vec<u8> {
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder.append_data(&mut header, name, contents).unwrap();
    builder.into_inner().unwrap()
}

/// Entries of a tar archive: path, mode and contents.
pub(crate) fn tar_entries(archive: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
    let mut archive = tar::Archive::new(archive);
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().display().to_string();
            let mode = entry.header().mode().unwrap();
            let mut contents = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut contents).unwrap();
            (path, mode, contents)
        })
        .collect()
}

/// A canned response.
#[derive(Debug, Clone)]
pub(crate) struct Reply {
    /// HTTP status.
    status: u16,
    /// Media type of the body.
    content_type: &'static str,
    /// Response body.
    body: Vec<u8>,
    /// Delay before answering.
    delay: Option<Duration>,
    /// Delay between the head and the body, during which the client holds an
    /// open response whose body has not arrived.
    body_delay: Option<Duration>,
    /// Whether the head announces one byte more than the body holds, so that
    /// the client sees the connection close in the middle of the body.
    truncated: bool,
    /// For `101 Switching Protocols` replies: extra headers. The body is then
    /// written as raw bytes after the head, and the client input is recorded
    /// until it closes its side.
    upgrade: Option<Vec<(String, String)>>,
    /// How an upgraded connection ends.
    hangup: Hangup,
}

/// How the fake daemon ends an upgraded connection.
#[derive(Debug, Clone, Copy, Default)]
enum Hangup {
    /// Once the client closes its side, after reading everything it sent.
    #[default]
    AfterInput,
    /// As a process that exits without reading its input: the output ends at
    /// once and the connection closes a moment later, the input unread.
    IgnoringInput,
    /// As a daemon that goes away: the connection closes a moment later
    /// without ending the output, the input unread.
    Reset,
}

/// How long a hanging-up upgraded connection stays open.
const HANGUP_DELAY: Duration = Duration::from_millis(200);

impl Reply {
    /// A JSON response.
    pub(crate) fn json(status: u16, body: &Value) -> Self {
        Self::raw(
            status,
            "application/json",
            serde_json::to_vec(body).unwrap(),
        )
    }

    /// An NDJSON response made of the given lines.
    pub(crate) fn ndjson(lines: &[Value]) -> Self {
        let mut body = Vec::new();
        for line in lines {
            serde_json::to_writer(&mut body, line).unwrap();
            body.push(b'\n');
        }
        Self::raw(200, "application/x-ndjson", body)
    }

    /// A response with an arbitrary body.
    pub(crate) fn raw(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
            delay: None,
            body_delay: None,
            truncated: false,
            upgrade: None,
            hangup: Hangup::AfterInput,
        }
    }

    /// Switches protocols, sends `headers`, then streams `payload` as raw bytes.
    pub(crate) fn upgrade(
        protocol: &str,
        headers: &[(&str, &str)],
        payload: impl Into<Vec<u8>>,
    ) -> Self {
        let mut all = vec![("upgrade".to_owned(), protocol.to_owned())];
        all.extend(
            headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        Self {
            upgrade: Some(all),
            ..Self::raw(101, "application/vnd.docker.raw-stream", payload)
        }
    }

    /// An empty response with `status`.
    pub(crate) fn empty(status: u16) -> Self {
        Self::raw(status, "text/plain", Vec::new())
    }

    /// Waits before answering.
    pub(crate) fn delayed(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Sends the head at once and the body after `delay`.
    pub(crate) fn with_body_after(mut self, delay: Duration) -> Self {
        self.body_delay = Some(delay);
        self
    }

    /// Closes the connection one byte short of the announced body.
    pub(crate) fn truncated(mut self) -> Self {
        self.truncated = true;
        self
    }

    /// Ends an upgraded connection as a process that does not read its input.
    pub(crate) fn ignoring_input(mut self) -> Self {
        self.hangup = Hangup::IgnoringInput;
        self
    }

    /// Drops an upgraded connection without ending the output first.
    pub(crate) fn resetting(mut self) -> Self {
        self.hangup = Hangup::Reset;
        self
    }
}

/// A request received by the fake daemon.
#[derive(Debug, Clone)]
pub(crate) struct Recorded {
    /// HTTP method.
    pub method: String,
    /// Path and query, as sent.
    pub target: String,
    /// Headers, names lowercased.
    pub headers: HashMap<String, String>,
    /// Request body.
    pub body: Vec<u8>,
    /// Bytes written by the client after a protocol switch.
    pub upgrade_input: Vec<u8>,
}

impl Recorded {
    /// Path without the query string.
    pub(crate) fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    /// Decoded query parameters.
    pub(crate) fn query(&self) -> HashMap<String, String> {
        self.target
            .split_once('?')
            .map(|(_, q)| q)
            .unwrap_or_default()
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (decode(k), decode(v))
            })
            .collect()
    }

    /// Body decoded as JSON.
    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

/// Percent-decodes a query component.
fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// The replies of one route, served in turn.
#[derive(Debug)]
struct Route {
    /// Replies in serving order; the last one answers every later request.
    replies: Vec<Reply>,
    /// Requests answered so far.
    served: AtomicUsize,
}

impl Route {
    /// The reply to the next request.
    fn next(&self) -> Reply {
        let n = self.served.fetch_add(1, Ordering::Relaxed);
        self.replies[n.min(self.replies.len() - 1)].clone()
    }
}

/// Builder of a [`FakeDaemon`].
#[derive(Default)]
pub(crate) struct FakeDaemonBuilder {
    /// Replies of each `(method, path)`.
    routes: HashMap<(String, String), Route>,
}

impl FakeDaemonBuilder {
    /// Answers `method path` (query ignored) with `reply`. A `*` segment of
    /// `path` matches any segment.
    pub(crate) fn route(self, method: &str, path: &str, reply: Reply) -> Self {
        self.route_sequence(method, path, vec![reply])
    }

    /// Answers successive requests for `method path` with `replies` in turn,
    /// the last one answering every later request.
    ///
    /// # Panics
    ///
    /// When `replies` is empty.
    pub(crate) fn route_sequence(mut self, method: &str, path: &str, replies: Vec<Reply>) -> Self {
        assert!(!replies.is_empty(), "a route needs a reply");
        let route = Route {
            replies,
            served: AtomicUsize::new(0),
        };
        self.routes
            .insert((method.to_owned(), path.to_owned()), route);
        self
    }

    /// Answers the health endpoint with the given API version.
    pub(crate) fn health(self, api_version: &str) -> Self {
        self.route(
            "GET",
            "/daemon/health",
            Reply::json(
                200,
                &serde_json::json!({
                    "status": "healthy",
                    "version": "v0.54.0",
                    "api_version": api_version,
                    "release": true,
                    "revision": "test",
                    "runtime_backend": false
                }),
            ),
        )
    }

    /// Starts serving on a fresh socket in a temporary directory.
    ///
    /// Must be called from within a tokio runtime.
    pub(crate) fn start(self) -> FakeDaemon {
        let dir = tempfile::tempdir().expect("temporary directory");
        let socket = dir.path().join("sandboxd.sock");
        let listener = UnixListener::bind(&socket).expect("bind fake daemon socket");
        let routes = Arc::new(self.routes);
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let task = {
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let routes = Arc::clone(&routes);
                    let recorded = Arc::clone(&recorded);
                    tokio::spawn(async move {
                        if let Some(request) = read_request(stream, &routes).await {
                            recorded.lock().unwrap().push(request);
                        }
                    });
                }
            })
        };

        FakeDaemon {
            socket,
            recorded,
            _dir: dir,
            task,
        }
    }
}

/// A fake `sandboxd`, stopped when dropped.
pub(crate) struct FakeDaemon {
    /// Socket the daemon listens on.
    socket: PathBuf,
    /// Requests received so far.
    recorded: Arc<Mutex<Vec<Recorded>>>,
    /// Directory of the socket, removed with the daemon.
    _dir: tempfile::TempDir,
    /// Accept loop.
    task: JoinHandle<()>,
}

impl FakeDaemon {
    /// A daemon answering the health route with a supported version.
    pub(crate) fn builder() -> FakeDaemonBuilder {
        FakeDaemonBuilder::default().health("0.52.0")
    }

    /// Path of the socket the daemon listens on.
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    /// Requests received so far, excluding health checks.
    pub(crate) fn requests(&self) -> Vec<Recorded> {
        self.recorded
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path() != "/daemon/health")
            .cloned()
            .collect()
    }

    /// The single request received for `method path`.
    pub(crate) fn request(&self, method: &str, path: &str) -> Recorded {
        let matching: Vec<Recorded> = self
            .requests()
            .into_iter()
            .filter(|r| r.method == method && r.path() == path)
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "expected exactly one {method} {path}, got {matching:?}"
        );
        matching.into_iter().next().unwrap()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Reads one HTTP/1.1 request, answers it and returns what was received.
async fn read_request(
    mut stream: UnixStream,
    routes: &HashMap<(String, String), Route>,
) -> Option<Recorded> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let (method, target) = (
        request_line.next()?.to_owned(),
        request_line.next()?.to_owned(),
    );
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();

    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }

    let path = target.split('?').next().unwrap_or_default().to_owned();
    let reply = find_route(routes, &method, &path).map_or_else(
        || Reply::raw(404, "text/plain", "404 page not found"),
        Route::next,
    );
    if let Some(delay) = reply.delay {
        tokio::time::sleep(delay).await;
    }
    if let Some(upgrade_headers) = &reply.upgrade {
        let mut head = String::from("HTTP/1.1 101 Switching Protocols\r\nconnection: Upgrade\r\n");
        for (name, value) in upgrade_headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await.ok()?;
        stream.write_all(&reply.body).await.ok()?;
        let mut upgrade_input = Vec::new();
        match reply.hangup {
            Hangup::AfterInput => {
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    stream.read_to_end(&mut upgrade_input),
                )
                .await;
                let _ = stream.shutdown().await;
            }
            Hangup::IgnoringInput => {
                let _ = stream.shutdown().await;
                tokio::time::sleep(HANGUP_DELAY).await;
            }
            Hangup::Reset => tokio::time::sleep(HANGUP_DELAY).await,
        }
        return Some(Recorded {
            method,
            target,
            headers,
            body,
            upgrade_input,
        });
    }
    let status_line = format!(
        "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        reply.body.len() + usize::from(reply.truncated)
    );
    let _ = stream.write_all(status_line.as_bytes()).await;
    if let Some(delay) = reply.body_delay {
        let _ = stream.flush().await;
        tokio::time::sleep(delay).await;
    }
    let _ = stream.write_all(&reply.body).await;
    let _ = stream.shutdown().await;

    Some(Recorded {
        method,
        target,
        headers,
        body,
        upgrade_input: Vec::new(),
    })
}

/// The route of `method path`: an exact match, else a route whose `*`
/// segments match any segment.
fn find_route<'a>(
    routes: &'a HashMap<(String, String), Route>,
    method: &str,
    path: &str,
) -> Option<&'a Route> {
    routes
        .get(&(method.to_owned(), path.to_owned()))
        .or_else(|| {
            routes.iter().find_map(|((m, pattern), route)| {
                let segments_match = pattern.split('/').count() == path.split('/').count()
                    && pattern
                        .split('/')
                        .zip(path.split('/'))
                        .all(|(p, s)| p == "*" || p == s);
                (m == method && segments_match).then_some(route)
            })
        })
}
