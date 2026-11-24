//! The state of the daemon, recorded before work that changes it and checked
//! afterwards.
//!
//! An audit, a suite replay, the live tests and the examples all create and
//! remove sandboxes, images, rules, secrets and MCP servers on the daemon of the
//! user. `audit state --save` records what the daemon holds; `audit state
//! --check` tells what differs since. The daemon also removes assembled kit
//! images on its own: the check reports the runs of that collector found in its
//! log since the record, so that an image it took is not blamed on the work.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::SockClient;
use crate::snapshot::DaemonState;

/// Message of the image collector of the daemon in its log.
const IMAGE_GC: &str = "assembled kit image gc";

/// A recorded state, and where the log of the daemon stood then.
#[derive(Debug, Serialize, Deserialize)]
struct Recorded {
    /// What the daemon held.
    daemon: DaemonState,
    /// Length of the daemon log at the time: the entries after it are new.
    log_len: u64,
}

/// Prints the state of the daemon, records it into `file` with `save`, or
/// compares it with the one recorded there with `check`.
///
/// # Errors
///
/// Fails when the daemon cannot be read, the record cannot be written or read,
/// or, with `check`, when the state differs.
pub(crate) async fn run(
    sock: &Path,
    sbx: &str,
    file: &Path,
    save: bool,
    check: bool,
) -> Result<()> {
    let daemon = DaemonState::read(&SockClient::new(sock), sbx).await?;
    let log = daemon_log(sock);
    if check {
        let text = std::fs::read_to_string(file).with_context(|| {
            format!(
                "no recorded state in {}: run `audit state --save` first",
                file.display()
            )
        })?;
        let recorded: Recorded =
            serde_json::from_str(&text).context("reading the recorded state")?;
        for line in image_gc_runs(&log_since(&log, recorded.log_len)) {
            eprintln!("[state] {line}");
        }
        let differences = recorded.daemon.differences(&daemon);
        if differences.is_empty() {
            eprintln!("[state] the daemon is as recorded");
            return Ok(());
        }
        for line in &differences {
            eprintln!("[state] !! {line}");
        }
        anyhow::bail!(
            "the daemon state differs from the record ({} differences)",
            differences.len()
        );
    }
    for line in daemon.summary() {
        println!("{line}");
    }
    if save {
        let recorded = Recorded {
            daemon,
            log_len: std::fs::metadata(&log).map_or(0, |m| m.len()),
        };
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(file, serde_json::to_vec_pretty(&recorded)?)
            .with_context(|| file.display().to_string())?;
        eprintln!("[state] recorded into {}", file.display());
    }
    Ok(())
}

/// The log of the daemon, next to its socket.
fn daemon_log(sock: &Path) -> std::path::PathBuf {
    sock.with_file_name("daemon.log")
}

/// What the log at `path` holds after its first `offset` bytes, all of it when
/// it is shorter (it was rotated); empty when it cannot be read.
fn log_since(path: &Path, offset: u64) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let start = if len < offset { 0 } else { offset };
    let mut text = String::new();
    if file.seek(SeekFrom::Start(start)).is_ok() {
        let _ = file.read_to_string(&mut text);
    }
    text
}

/// One line per run of the image collector in `log`, a daemon log of JSON
/// lines.
fn image_gc_runs(log: &str) -> Vec<String> {
    log.lines()
        .filter(|line| line.contains(IMAGE_GC))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["msg"] == IMAGE_GC)
        .map(|entry| {
            format!(
                "the daemon removed assembled kit images on its own at {} (removed {}, kept {}): assembled tags gone since are its doing, the kit images they pointed to stay untagged",
                entry["time"].as_str().unwrap_or("?"),
                entry["removed"],
                entry["kept"]
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_runs_are_read_from_the_log() {
        let log = concat!(
            r#"{"time":"2026-11-24T16:47:37+01:00","level":"INFO","msg":"http request"}"#,
            "\n",
            r#"{"time":"2026-11-24T16:54:57+01:00","level":"INFO","msg":"assembled kit image gc","removed":3,"kept":3}"#,
            "\n",
            "not json, assembled kit image gc\n",
        );
        assert_eq!(
            image_gc_runs(log),
            [
                "the daemon removed assembled kit images on its own at 2026-11-24T16:54:57+01:00 (removed 3, kept 3): assembled tags gone since are its doing, the kit images they pointed to stay untagged"
            ]
        );
        assert!(image_gc_runs("").is_empty());
    }
}
