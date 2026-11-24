//! Spy proxy between the `sbx` CLI and `sandboxd`.
//!
//! The proxy listens on its own Unix socket and opens one upstream connection to the daemon
//! socket for every client connection. Bytes are relayed unchanged in both directions; in
//! parallel, each direction is fed to an [`HttpTap`] that decodes HTTP/1.1 (message heads, then
//! bodies framed by `Content-Length`, `Transfer-Encoding: chunked` or connection close) and
//! records every exchange as one JSON object per line (JSONL) in the capture file.
//!
//! Point the CLI at the proxy with `DOCKER_SANDBOXES_API=<listen socket>` to capture its traffic.
//!
//! # Captured events
//!
//! Every line carries a `kind` and a `ts` (milliseconds since the Unix epoch):
//!
//! | `kind` | Other fields |
//! |---|---|
//! | `conn_open`, `conn_close` | `conn`, `tag` |
//! | `conn_error` | `error` |
//! | `request` | `method`, `path`, then the message fields |
//! | `response` | `status`, then the message fields |
//! | `raw` | `dir`, `conn`, `tag`, `len`, `preview` |
//! | `raw_summary` | `dir`, `conn`, `tag`, `bytes` |
//!
//! The message fields are `conn`, `seq`, `tag`, `headers` and `body_len`, plus `body` when the
//! body is not empty and `body_truncated` when it exceeded the size limit. Requests and
//! responses are numbered independently on each connection, so the response with a given
//! `seq` answers the request with the same `seq`.
//!
//! Bodies are stored as parsed JSON when possible, as `{"ndjson": [...]}` for newline-delimited
//! JSON streams, as a plain string for other UTF-8 text, and as `{"binary_preview": "..."}`
//! otherwise. Only the first [`SpyState::max_body`] bytes of a body are kept.
//!
//! # Redaction
//!
//! When redaction is enabled, sensitive header values and sensitive JSON string values are
//! replaced with `<redacted:N>`, where `N` is the length of the original value in bytes.
//!
//! Text bodies that are not valid JSON, such as a JSON document cut at the size limit, are
//! masked by pattern instead.
//!
//! # Known limitations
//!
//! The decoder targets the traffic of the `sbx` CLI. It does not handle responses to `HEAD`
//! requests, interim `1xx` responses other than `101`, more than 96 headers per message, or
//! malformed chunk framing; such exchanges may be recorded incorrectly, while the relayed bytes
//! are never altered.
//!
//! # Upgraded and hijacked streams
//!
//! After a `101 Switching Protocols` response, or a Docker-style hijacked stream (raw or
//! multiplexed stream content type, or a `200` answer to an upgrade request with no body
//! framing), both directions of the connection switch to raw mode: HTTP parsing stops and
//! the traffic is recorded as escaped `raw` previews followed by a `raw_summary`.

use crate::privacy::Scrubber;
use regex::Regex;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};

/// State shared by every connection handled by the spy.
pub(crate) struct SpyState {
    /// Label attached to the traffic of new connections (`manual` by default).
    pub tag: Mutex<String>,
    /// Number of connections opened so far; also the source of connection identifiers.
    pub conns: AtomicU64,
    /// Whether sensitive header and JSON values are masked in the capture.
    pub redact: bool,
    /// Maximum number of body bytes kept per message (bytes beyond are counted, not stored).
    pub max_body: usize,
    /// When set, the tag is re-read from this file on every new connection.
    pub tag_file: Mutex<Option<PathBuf>>,
    /// Line-buffered writer for the JSONL capture file.
    log: Mutex<std::io::LineWriter<std::fs::File>>,
    /// Removes personal data from every event before it is written.
    scrubber: Arc<Scrubber>,
}

impl SpyState {
    /// Opens (or creates) the capture file at `log_path` in append mode and returns the
    /// shared state.
    ///
    /// Missing parent directories are created. The initial tag is `manual` and the body size
    /// limit is 256 KiB. Every event goes through `scrubber` before it is written.
    pub(crate) fn new(
        log_path: &Path,
        redact: bool,
        scrubber: Arc<Scrubber>,
    ) -> anyhow::Result<Arc<Self>> {
        if let Some(p) = log_path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        Ok(Arc::new(Self {
            tag: Mutex::new("manual".into()),
            conns: AtomicU64::new(0),
            redact,
            max_body: 256 * 1024,
            tag_file: Mutex::new(None),
            log: Mutex::new(std::io::LineWriter::new(f)),
            scrubber,
        }))
    }

    /// Sets the tag applied to connections opened from now on.
    pub(crate) fn set_tag(&self, t: &str) {
        *self.tag.lock().unwrap_or_else(PoisonError::into_inner) = t.to_string();
    }

    /// Sets the file the tag is re-read from on every new connection.
    ///
    /// A readable, non-empty tag file takes precedence over the tag set with
    /// [`SpyState::set_tag`].
    pub(crate) fn set_tag_file(&self, p: PathBuf) {
        *self.tag_file.lock().unwrap_or_else(PoisonError::into_inner) = Some(p);
    }

    /// Returns the tag for a new connection: the trimmed content of the tag file when it is
    /// set, readable and non-empty, otherwise the current in-memory tag.
    fn current_tag(&self) -> String {
        if let Some(p) = self
            .tag_file
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            && let Ok(t) = std::fs::read_to_string(p)
        {
            let t = t.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
        self.tag
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Adds a `ts` field (milliseconds since the Unix epoch) to `v`, scrubs it and appends
    /// it to the capture file as one JSON line.
    ///
    /// Write errors are ignored: capturing must never interrupt the relayed traffic.
    pub(crate) fn event(&self, mut v: Value) {
        self.scrubber.value(&mut v);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        v["ts"] = json!(ts);
        let mut w = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(w, "{v}");
    }
}

/// Listens on `listen` and relays every accepted connection to the `upstream` socket.
///
/// A stale socket file at `listen` is removed first, and the new socket is restricted to its
/// owner (mode `0600`). Each connection is handled in its own task; a connection that fails
/// is recorded as a `conn_error` event. Once the socket is set up, this function only returns
/// on an accept error.
pub(crate) async fn serve(
    listen: PathBuf,
    upstream: PathBuf,
    st: Arc<SpyState>,
) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(&listen);
    let l = UnixListener::bind(&listen)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&listen, std::fs::Permissions::from_mode(0o600))?;
    }
    eprintln!(
        "[spy] listening on {} → {}",
        listen.display(),
        upstream.display()
    );
    loop {
        let (c, _) = l.accept().await?;
        let (st, up) = (st.clone(), upstream.clone());
        tokio::spawn(async move {
            if let Err(e) = handle(c, up, st.clone()).await {
                st.event(json!({"kind": "conn_error", "error": format!("{e:#}")}));
            }
        });
    }
}

/// Relays one client connection to the daemon until both directions are closed.
///
/// The connection identifier and tag are assigned once the upstream connection succeeds.
/// The two taps share two flags: `raw`, raised by the response tap when the stream is
/// upgraded or hijacked so that the request tap stops parsing HTTP too, and `upgrade_req`,
/// raised by the request tap when a request carries an `Upgrade` header so that the
/// response tap can recognize a hijacked `200` response.
async fn handle(client: UnixStream, upstream: PathBuf, st: Arc<SpyState>) -> anyhow::Result<()> {
    let server = UnixStream::connect(&upstream).await?;
    let conn = st.conns.fetch_add(1, Ordering::SeqCst);
    let tag = st.current_tag();
    st.event(json!({"kind": "conn_open", "conn": conn, "tag": tag}));

    let raw = Arc::new(AtomicBool::new(false));
    let upgrade_req = Arc::new(AtomicBool::new(false));
    let (cr, cw) = client.into_split();
    let (sr, sw) = server.into_split();

    let req_tap = HttpTap::new(
        Dir::Request,
        conn,
        &tag,
        st.clone(),
        raw.clone(),
        upgrade_req.clone(),
    );
    let resp_tap = HttpTap::new(Dir::Response, conn, &tag, st.clone(), raw, upgrade_req);
    let _ = tokio::join!(pump(cr, sw, req_tap), pump(sr, cw, resp_tap));
    st.event(json!({"kind": "conn_close", "conn": conn, "tag": tag}));
    Ok(())
}

/// Copies bytes from `r` to `w` unchanged, feeding each chunk to `tap` once it is written.
///
/// On end of stream, the tap flushes any pending message and the write half is shut down so
/// that the peer sees the half-close.
async fn pump(mut r: OwnedReadHalf, mut w: OwnedWriteHalf, mut tap: HttpTap) -> anyhow::Result<()> {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            tap.finish();
            let _ = w.shutdown().await;
            return Ok(());
        }
        w.write_all(&buf[..n]).await?;
        tap.feed(&buf[..n]);
    }
}

/// Direction of the traffic observed by a tap.
#[derive(Clone, Copy, PartialEq)]
enum Dir {
    /// From the CLI to the daemon.
    Request,
    /// From the daemon to the CLI.
    Response,
}

/// Position inside a `Transfer-Encoding: chunked` body.
#[derive(Clone, Copy)]
enum Chunk {
    /// Waiting for a chunk-size line.
    Size,
    /// Inside chunk data, with this many bytes left.
    Data(usize),
    /// Waiting for the CRLF that ends the chunk data.
    DataCrlf,
    /// Reading trailer lines after the last chunk, up to the final empty line.
    Trailer,
}

/// State of the HTTP decoder of a tap.
enum TapState {
    /// Waiting for, or parsing, a message head (request line or status line, then headers).
    Head,
    /// Reading a `Content-Length` body with this many bytes left.
    Fixed(usize),
    /// Reading a chunked body.
    Chunked(Chunk),
    /// Reading a response body delimited by the end of the connection.
    UntilClose,
    /// No longer parsing HTTP: bytes are recorded as `raw` events.
    Raw,
}

/// Passive HTTP/1.1 decoder for one direction of one connection.
///
/// The tap only observes the relayed bytes: it never alters or delays them. It turns each
/// message into a `request` or `response` event, or into `raw` events once the stream is no
/// longer HTTP.
struct HttpTap {
    /// Direction observed by this tap.
    dir: Dir,
    /// Connection identifier written as `conn`.
    conn: u64,
    /// Tag of the connection written as `tag`.
    tag: String,
    /// Shared spy state (capture file and settings).
    st: Arc<SpyState>,
    /// Raised by the response tap when the stream is upgraded or hijacked; shared by both taps.
    raw: Arc<AtomicBool>,
    /// Raised by the request tap when a request asks for an upgrade; shared by both taps.
    upgrade_req: Arc<AtomicBool>,
    /// Current decoder state.
    state: TapState,
    /// Bytes received but not consumed by the decoder yet.
    buf: Vec<u8>,
    /// Event built from the current message head, written once its body is complete.
    pending: Option<Value>,
    /// Captured body bytes, capped at [`SpyState::max_body`].
    body: Vec<u8>,
    /// Total body length seen so far, including bytes beyond the cap.
    body_len: usize,
    /// Sequence number of the next message in this direction.
    seq: u64,
    /// Number of `raw` events written so far (capped at 200).
    raw_events: u32,
    /// Total number of bytes seen in raw mode.
    raw_bytes: u64,
}

impl HttpTap {
    /// Creates a tap in the [`TapState::Head`] state for the given direction and connection.
    fn new(
        dir: Dir,
        conn: u64,
        tag: &str,
        st: Arc<SpyState>,
        raw: Arc<AtomicBool>,
        upgrade_req: Arc<AtomicBool>,
    ) -> Self {
        Self {
            dir,
            conn,
            tag: tag.into(),
            st,
            raw,
            upgrade_req,
            state: TapState::Head,
            buf: vec![],
            pending: None,
            body: vec![],
            body_len: 0,
            seq: 0,
            raw_events: 0,
            raw_bytes: 0,
        }
    }

    /// Returns the direction as written in events (`request` or `response`).
    fn dir_str(&self) -> &'static str {
        match self.dir {
            Dir::Request => "request",
            Dir::Response => "response",
        }
    }

    /// Appends `data` to the input buffer and advances the decoder as far as possible.
    ///
    /// Bytes that cannot be parsed as an HTTP message head switch the tap to raw mode, as
    /// does the shared `raw` flag raised by the other direction.
    fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        loop {
            match self.state {
                TapState::Raw => {
                    self.log_raw();
                    return;
                }
                TapState::Head => {
                    if self.buf.is_empty() {
                        return;
                    }
                    if self.raw.load(Ordering::SeqCst) {
                        self.state = TapState::Raw;
                        continue;
                    }
                    match self.parse_head() {
                        Some(true) => {}
                        Some(false) => return, // Incomplete head: wait for more data.
                        None => self.state = TapState::Raw,
                    }
                }
                TapState::Fixed(n) => {
                    let take = n.min(self.buf.len());
                    let chunk: Vec<u8> = self.buf.drain(..take).collect();
                    self.push_body(&chunk);
                    if n - take == 0 {
                        self.emit();
                    } else {
                        self.state = TapState::Fixed(n - take);
                        return;
                    }
                }
                TapState::UntilClose => {
                    let chunk: Vec<u8> = self.buf.drain(..).collect();
                    self.push_body(&chunk);
                    return;
                }
                TapState::Chunked(c) => {
                    if !self.step_chunked(c) {
                        return;
                    }
                }
            }
        }
    }

    /// Parses a message head at the start of the input buffer.
    ///
    /// Returns `Some(true)` when a head was consumed (the next state is set, and a message
    /// without a body is already written), `Some(false)` when the head is incomplete, and
    /// `None` when the bytes are not HTTP.
    ///
    /// For responses, this is also where upgraded and hijacked streams are detected.
    fn parse_head(&mut self) -> Option<bool> {
        let mut headers = [httparse::EMPTY_HEADER; 96];
        let (len, mut ev, hdrs): (usize, Value, Vec<(String, String)>) = match self.dir {
            Dir::Request => {
                let mut req = httparse::Request::new(&mut headers);
                match req.parse(&self.buf) {
                    Ok(httparse::Status::Complete(len)) => {
                        let hdrs = collect_headers(req.headers);
                        let ev = json!({
                            "method": req.method.unwrap_or(""),
                            "path": req.path.unwrap_or(""),
                        });
                        (len, ev, hdrs)
                    }
                    Ok(httparse::Status::Partial) => return Some(false),
                    Err(_) => return None,
                }
            }
            Dir::Response => {
                let mut resp = httparse::Response::new(&mut headers);
                match resp.parse(&self.buf) {
                    Ok(httparse::Status::Complete(len)) => {
                        let hdrs = collect_headers(resp.headers);
                        let ev = json!({ "status": resp.code.unwrap_or(0) });
                        (len, ev, hdrs)
                    }
                    Ok(httparse::Status::Partial) => return Some(false),
                    Err(_) => return None,
                }
            }
        };
        self.buf.drain(..len);

        // Case-insensitive header lookup; the value is lowercased.
        let get = |k: &str| {
            hdrs.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.to_ascii_lowercase())
        };
        let chunked = get("transfer-encoding").is_some_and(|v| v.contains("chunked"));
        let cl = get("content-length").and_then(|v| v.trim().parse::<usize>().ok());
        let ctype = get("content-type").unwrap_or_default();

        ev["kind"] = json!(self.dir_str());
        ev["conn"] = json!(self.conn);
        ev["seq"] = json!(self.seq);
        ev["tag"] = json!(self.tag);
        ev["headers"] = json!(redact_headers(&hdrs, self.st.redact));
        self.seq += 1;
        self.body.clear();
        self.body_len = 0;

        match self.dir {
            Dir::Request => {
                if get("upgrade").is_some() {
                    self.upgrade_req.store(true, Ordering::SeqCst);
                }
                self.pending = Some(ev);
                // A request without chunked encoding or a non-zero length has no body.
                self.state = match (chunked, cl) {
                    (true, _) => TapState::Chunked(Chunk::Size),
                    (false, Some(n)) if n > 0 => TapState::Fixed(n),
                    _ => {
                        self.emit();
                        return Some(true);
                    }
                };
            }
            Dir::Response => {
                let status = ev["status"].as_u64().unwrap_or(0);
                // Docker-style hijacked stream: a 101, a raw or multiplexed stream content
                // type, or a 200 without body framing in answer to an upgrade request.
                let hijack = status == 101
                    || ctype.contains("vnd.docker.raw-stream")
                    || ctype.contains("vnd.docker.multiplexed-stream")
                    || (self.upgrade_req.load(Ordering::SeqCst)
                        && status == 200
                        && cl.is_none()
                        && !chunked);
                self.pending = Some(ev);
                if hijack {
                    self.raw.store(true, Ordering::SeqCst);
                    self.emit();
                    self.state = TapState::Raw;
                    return Some(true);
                }
                // The upgrade was not granted: later responses on this keep-alive
                // connection must not be mistaken for a hijacked stream.
                self.upgrade_req.store(false, Ordering::SeqCst);
                // 204 and 304 never have a body; without any framing, the body lasts until
                // the connection closes.
                self.state = match (chunked, cl, status) {
                    (_, _, 204 | 304) | (false, Some(0), _) => {
                        self.emit();
                        return Some(true);
                    }
                    (true, _, _) => TapState::Chunked(Chunk::Size),
                    (false, Some(n), _) => TapState::Fixed(n),
                    (false, None, _) => TapState::UntilClose,
                };
            }
        }
        Some(true)
    }

    /// Advances the chunked body decoder by one step from position `c`.
    ///
    /// Returns `false` when more data is needed. Chunk extensions are ignored, and the message
    /// is written once the empty line ending the trailer section is reached.
    fn step_chunked(&mut self, c: Chunk) -> bool {
        match c {
            Chunk::Size => {
                let Some(pos) = find_crlf(&self.buf) else {
                    return false;
                };
                let line: Vec<u8> = self.buf.drain(..pos + 2).collect();
                let s = String::from_utf8_lossy(&line[..pos]);
                let hex = s.split(';').next().unwrap_or("").trim();
                let n = usize::from_str_radix(hex, 16).unwrap_or(0);
                self.state = if n == 0 {
                    TapState::Chunked(Chunk::Trailer)
                } else {
                    TapState::Chunked(Chunk::Data(n))
                };
                true
            }
            Chunk::Data(n) => {
                let take = n.min(self.buf.len());
                if take == 0 {
                    return false;
                }
                let chunk: Vec<u8> = self.buf.drain(..take).collect();
                self.push_body(&chunk);
                self.state = if n - take == 0 {
                    TapState::Chunked(Chunk::DataCrlf)
                } else {
                    TapState::Chunked(Chunk::Data(n - take))
                };
                true
            }
            Chunk::DataCrlf => {
                if self.buf.len() < 2 {
                    return false;
                }
                self.buf.drain(..2);
                self.state = TapState::Chunked(Chunk::Size);
                true
            }
            Chunk::Trailer => {
                let Some(pos) = find_crlf(&self.buf) else {
                    return false;
                };
                self.buf.drain(..pos + 2);
                if pos == 0 {
                    self.emit();
                }
                true
            }
        }
    }

    /// Counts `chunk` in the body length and keeps as much of it as the body cap allows.
    fn push_body(&mut self, chunk: &[u8]) {
        self.body_len += chunk.len();
        let room = self.st.max_body.saturating_sub(self.body.len());
        self.body.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    /// Completes the pending event with the body fields, writes it, and gets ready for the
    /// next message head.
    fn emit(&mut self) {
        if let Some(mut ev) = self.pending.take() {
            ev["body_len"] = json!(self.body_len);
            if !self.body.is_empty() {
                ev["body"] = render_body(&self.body, self.st.redact);
                if self.body_len > self.body.len() {
                    ev["body_truncated"] = json!(true);
                }
            }
            self.st.event(ev);
        }
        self.body.clear();
        self.body_len = 0;
        self.state = TapState::Head;
    }

    /// Records the buffered bytes as a `raw` event with an escaped preview of the first 256
    /// bytes, then clears the buffer.
    ///
    /// At most 200 `raw` events are written per tap; later bytes are only counted for the
    /// final `raw_summary`.
    fn log_raw(&mut self) {
        let n = self.buf.len();
        if n == 0 {
            return;
        }
        self.raw_bytes += n as u64;
        if self.raw_events < 200 {
            self.raw_events += 1;
            self.st.event(json!({
                "kind": "raw",
                "dir": self.dir_str(),
                "conn": self.conn,
                "tag": self.tag,
                "len": n,
                "preview": escape(&self.buf[..n.min(256)]),
            }));
        }
        self.buf.clear();
    }

    /// Handles the end of the stream: writes a message whose body was still being read, then
    /// a `raw_summary` event if any raw bytes were seen.
    fn finish(&mut self) {
        if matches!(
            self.state,
            TapState::UntilClose | TapState::Fixed(_) | TapState::Chunked(_)
        ) {
            self.emit();
        }
        if self.raw_bytes > 0 {
            self.st.event(json!({
                "kind": "raw_summary",
                "dir": self.dir_str(),
                "conn": self.conn,
                "tag": self.tag,
                "bytes": self.raw_bytes,
            }));
        }
    }
}

/// Converts parsed headers into owned `(name, value)` pairs, decoding values lossily as UTF-8.
fn collect_headers(h: &[httparse::Header<'_>]) -> Vec<(String, String)> {
    h.iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).to_string(),
            )
        })
        .collect()
}

/// Returns the offset of the first CRLF in `b`, if any.
fn find_crlf(b: &[u8]) -> Option<usize> {
    b.windows(2).position(|w| w == b"\r\n")
}

/// Renders bytes as printable ASCII: `\n` and `\r` are escaped, and other non-printable
/// bytes are written as `\xNN`.
fn escape(b: &[u8]) -> String {
    b.iter()
        .map(|&c| match c {
            b'\n' => "\\n".to_string(),
            b'\r' => "\\r".to_string(),
            0x20..=0x7e => (c as char).to_string(),
            _ => format!("\\x{c:02x}"),
        })
        .collect()
}

/// Builds the JSON object of headers written in events.
///
/// When `redact` is set, the values of `Authorization`, `Cookie`, `Proxy-Authorization` and
/// of any header whose name contains `cookie`, `token`, `secret`, `key` or `password` are
/// replaced with `<redacted:N>`.
fn redact_headers(h: &[(String, String)], redact: bool) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in h {
        let lk = k.to_ascii_lowercase();
        let sensitive = matches!(lk.as_str(), "authorization" | "proxy-authorization")
            || lk.contains("cookie")
            || lk.contains("token")
            || lk.contains("secret")
            || lk.contains("key")
            || lk.contains("password");
        let v = if redact && sensitive {
            format!("<redacted:{}>", v.len())
        } else {
            v.clone()
        };
        m.insert(k.clone(), json!(v));
    }
    Value::Object(m)
}

/// Converts a captured body into the JSON value written as `body`.
///
/// In order of preference: a JSON document, newline-delimited JSON wrapped as
/// `{"ndjson": [...]}`, a plain UTF-8 string, or `{"binary_preview": "..."}` holding the
/// escaped first 512 bytes. Redaction, when enabled, applies to the JSON forms and, by
/// pattern, to text bodies (see [`redact_text`]).
fn render_body(b: &[u8], redact: bool) -> Value {
    if let Ok(mut v) = serde_json::from_slice::<Value>(b) {
        if redact {
            redact_json(&mut v);
        }
        return v;
    }
    // NDJSON (event stream): one JSON value per non-empty line, and every line must parse.
    if let Ok(s) = std::str::from_utf8(b) {
        let lines: Vec<Value> = s
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if !lines.is_empty() && lines.len() == s.lines().filter(|l| !l.trim().is_empty()).count() {
            let mut v = Value::Array(lines);
            if redact {
                redact_json(&mut v);
            }
            return json!({ "ndjson": v });
        }
        return json!(if redact { redact_text(s) } else { s.to_owned() });
    }
    json!({ "binary_preview": escape(&b[..b.len().min(512)]) })
}

/// Masks JSON-looking `"key": "value"` pairs with sensitive keys in a body that
/// could not be parsed (typically a JSON document truncated to the size limit),
/// using the same key rules as [`redact_json`].
fn redact_text(text: &str) -> String {
    static PAIR: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let pair = PAIR.get_or_init(|| {
        Regex::new(r#"(?i)("(?:[^"]*(?:secret|password|passwd|token|apikey|api_key|authorization|credential)[^"]*|value)"\s*:\s*)"((?:[^"\\]|\\.)*)""#)
            .expect("valid regex")
    });
    pair.replace_all(text, |caps: &regex::Captures<'_>| {
        format!("{}\"<redacted:{}>\"", &caps[1], caps[2].len())
    })
    .into_owned()
}

/// Recursively replaces sensitive string values in `v` with `<redacted:N>`.
///
/// A value is sensitive when its key matches, case-insensitively, one of `secret`,
/// `password`, `passwd`, `token`, `apikey`, `api_key`, `authorization` or `credential`, or is
/// exactly `value` (as in `{"name": ..., "value": ...}` secret entries). Non-string values
/// under a sensitive key are not masked but are searched recursively.
fn redact_json(v: &mut Value) {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?i)(secret|password|passwd|token|apikey|api_key|authorization|credential|^value$)",
        )
        .expect("valid regex")
    });
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if re.is_match(k)
                    && let Value::String(s) = val
                {
                    *val = json!(format!("<redacted:{}>", s.len()));
                    continue;
                }
                redact_json(val);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(redact_json),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_json_bodies_are_masked() {
        let body = br#"{"name":"dummy","secrets":[{"name":"API_TOKEN","value":"s3cr3t"}],"password":"hunter2","tr"#;
        let rendered = render_body(body, true);
        let text = rendered
            .as_str()
            .expect("an unparsable body is kept as text");
        assert!(
            !text.contains("s3cr3t") && !text.contains("hunter2"),
            "{text}"
        );
        assert!(text.contains(r#""value":"<redacted:6>""#), "{text}");
        assert!(text.contains(r#""name":"dummy""#), "{text}");
        assert_eq!(
            render_body(body, false).as_str(),
            std::str::from_utf8(body).ok()
        );
    }

    #[test]
    fn sensitive_headers_are_masked() {
        let headers = vec![
            ("Set-Cookie".to_owned(), "id=1".to_owned()),
            ("X-Api-Key".to_owned(), "abc".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
        ];
        let masked = redact_headers(&headers, true);
        assert_eq!(masked["Set-Cookie"], "<redacted:4>");
        assert_eq!(masked["X-Api-Key"], "<redacted:3>");
        assert_eq!(masked["Content-Type"], "application/json");
    }
}
