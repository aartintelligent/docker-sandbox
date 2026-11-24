//! Direct requests to the daemon socket.
//!
//! These commands prove that an operation can be driven WITHOUT the CLI:
//!
//! - [`send`] issues an arbitrary request and prints the response, or, with an upgrade,
//!   bridges the hijacked exec/attach stream to the terminal;
//! - [`replay`] sends a request recorded by the spy straight to the socket.
//!
//! In an upgraded session, stdin is copied to the stream and the stream is copied to stdout.
//! With Docker `stdcopy` demultiplexing enabled, each frame is routed to stdout or stderr
//! according to its stream byte.

use crate::client::{SockClient, preview};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use serde_json::Value;
use std::io::BufRead;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Parses `Name: value` header arguments (as given with `-H`) into `(name, value)` pairs.
///
/// Each argument is split at its first colon and both parts are trimmed.
pub(crate) fn parse_headers(raw: &[String]) -> Result<Vec<(String, String)>> {
    raw.iter()
        .map(|h| {
            let (k, v) = h
                .split_once(':')
                .context("expected a header in the form `Name: value`")?;
            Ok((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// Loads the request body from the `--data` argument.
///
/// A value starting with `@` names a file whose content is used as the body; any other value
/// is used literally.
pub(crate) fn load_data(data: Option<&str>) -> Result<Option<Bytes>> {
    Ok(match data {
        None => None,
        Some(d) if d.starts_with('@') => Some(Bytes::from(std::fs::read(&d[1..])?)),
        Some(d) => Some(Bytes::from(d.to_string())),
    })
}

/// Sends a request to the socket and prints the response.
///
/// Without `upgrade`, the status line and headers are printed, followed by the body
/// (pretty-printed when it is JSON, otherwise a preview of up to 16 KiB). With `upgrade`, the
/// request is sent as a Docker-style upgrade and the resulting stream is bridged to the
/// terminal, demultiplexed when `stdcopy` is set.
pub(crate) async fn send(
    sock: &Path,
    method: &str,
    path: &str,
    headers: Vec<(String, String)>,
    body: Option<Bytes>,
    upgrade: bool,
    stdcopy: bool,
) -> Result<()> {
    let client = SockClient::new(sock);
    let req = SockClient::build(method, path, &headers, body)?;
    if upgrade {
        let io = client.upgrade(req).await?;
        return interactive(io, stdcopy).await;
    }
    let res = client.send(req).await?;
    println!("HTTP {}", res.status);
    for (k, v) in &res.headers {
        println!("{k}: {}", v.to_str().unwrap_or("<bin>"));
    }
    println!();
    match serde_json::from_slice::<Value>(&res.body) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
        Err(_) => println!("{}", preview(&res.body, 16 * 1024)),
    }
    Ok(())
}

/// Bridges an upgraded stream to the terminal: stdin is copied to the stream, and the stream
/// is copied to stdout until the daemon closes it.
///
/// When `stdcopy` is set, the output is demultiplexed from Docker frames, each made of an
/// 8-byte header `[stream: u8, 0, 0, 0, size: u32 big-endian]` followed by `size` bytes of
/// payload. Stream 2 goes to stderr, stream 3 (exec result or system error) goes to stderr
/// with a `[systemerr]` prefix, and the others go to stdout. Data that does not look like a
/// frame header is written to stdout as is.
async fn interactive<T>(io: T, stdcopy: bool) -> Result<()>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(io);

    // stdin → stream. The terminal is read on a dedicated thread: a blocking
    // read cannot be cancelled, and a runtime-managed one would keep the process
    // alive until the user presses Enter once the session is over.
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match std::io::Read::read(&mut stdin, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if input_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    // Half-close the stream when stdin reaches end of file.
    let up = tokio::spawn(async move {
        while let Some(chunk) = input_rx.recv().await {
            if wr.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    // Stream → stdout/stderr. Once a non-stdcopy frame is seen, the rest of the
    // stream is passed through unchanged.
    let mut out = tokio::io::stdout();
    let mut err = tokio::io::stderr();
    let mut raw_mode = !stdcopy;
    let mut acc: Vec<u8> = vec![];
    let mut buf = [0u8; 8192];
    loop {
        let n = rd.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if raw_mode {
            out.write_all(&buf[..n]).await?;
            out.flush().await?;
            continue;
        }
        acc.extend_from_slice(&buf[..n]);
        while acc.len() >= 8 {
            let stream = acc[0];
            if stream > 3 || acc[1..4] != [0, 0, 0] {
                // Not a stdcopy frame (most likely a raw TTY stream).
                raw_mode = true;
                out.write_all(&acc).await?;
                acc.clear();
                break;
            }
            let size = u32::from_be_bytes([acc[4], acc[5], acc[6], acc[7]]) as usize;
            if acc.len() < 8 + size {
                break;
            }
            let payload: Vec<u8> = acc.drain(..8 + size).skip(8).collect();
            match stream {
                2 => err.write_all(&payload).await?,
                3 => {
                    // Docker uses stream 3 for the exec result or a system error.
                    err.write_all(b"[systemerr] ").await?;
                    err.write_all(&payload).await?;
                }
                _ => out.write_all(&payload).await?,
            }
        }
        out.flush().await?;
    }
    // Bytes too short to form a frame header are output as they are.
    if !acc.is_empty() {
        out.write_all(&acc).await?;
        out.flush().await?;
    }
    up.abort();
    Ok(())
}

/// Replays the request identified by (`conn`, `seq`) from a spy capture directly on the socket.
///
/// The first matching `request` event of the capture is used. Its `Host` header is dropped
/// (the client sets its own), and a captured `Upgrade` header turns the replay into an
/// upgraded session with `stdcopy` demultiplexing. The request is printed first, with a
/// warning when it contains redacted values; with `dry_run`, nothing is sent.
pub(crate) async fn replay(
    sock: &Path,
    capture: &Path,
    conn: u64,
    seq: u64,
    dry_run: bool,
) -> Result<()> {
    let f = std::fs::File::open(capture)?;
    let mut found: Option<Value> = None;
    for line in std::io::BufReader::new(f).lines() {
        let Ok(v) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if v["kind"] == "request" && v["conn"] == conn && v["seq"] == seq {
            found = Some(v);
            break;
        }
    }
    let Some(req) = found else {
        bail!("request conn={conn} seq={seq} not found in the capture")
    };

    let method = req["method"].as_str().unwrap_or("GET");
    let path = req["path"].as_str().unwrap_or("/");
    let headers: Vec<(String, String)> = req["headers"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !k.eq_ignore_ascii_case("host"))
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    // Bodies the spy did not store verbatim cannot be replayed faithfully.
    let wrapped =
        req["body"].get("ndjson").is_some() || req["body"].get("binary_preview").is_some();
    if wrapped || req["body_truncated"] == true {
        bail!(
            "request conn={conn} seq={seq} cannot be replayed: its body was not captured verbatim (truncated, NDJSON or binary)"
        );
    }
    // A captured body is either a JSON string (sent as is) or a JSON value (re-serialized).
    let body = match &req["body"] {
        Value::Null => None,
        Value::String(s) => Some(Bytes::from(s.clone())),
        v => Some(Bytes::from(serde_json::to_vec(v)?)),
    };
    let redacted = body
        .as_ref()
        .is_some_and(|b| String::from_utf8_lossy(b).contains("<redacted:"))
        || headers.iter().any(|(_, v)| v.contains("<redacted:"));

    println!("→ {method} {path}  (tag={})", req["tag"]);
    if let Some(b) = &body {
        println!("{}", preview(b, 8000));
    }
    if redacted {
        println!(
            "!! The request contains redacted values: recapture with --no-redact (dummy values only)."
        );
    }
    if dry_run {
        return Ok(());
    }
    let upgrade = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("upgrade"));
    send(sock, method, path, headers, body, upgrade, upgrade).await
}
