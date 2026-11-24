//! Who is allowed to talk to the `sandboxd` socket, and who is
//! listening behind it?
//!
//! [`run`] gathers the facts needed to answer these questions:
//!
//! - the metadata of the socket file itself (type, mode, owner);
//! - the metadata of every parent directory up to `/`, since directory
//!   permissions also gate access to the socket;
//! - the identity of the daemon process, obtained through `SO_PEERCRED` and
//!   completed with its command line, executable and a filtered view of its
//!   environment read from `/proc/<pid>`.
//!
//! No request is sent to the daemon: the module only connects to the socket to
//! read the peer credentials.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use tokio::net::UnixStream;

/// Prefixes of the daemon environment variables included in the report.
///
/// Matching uses `starts_with`, so `XDG_` covers every XDG variable and
/// `DOCKER_SANDBOXES` covers the whole `DOCKER_SANDBOXES_*` family. Every other
/// variable is left out of the report.
const ENV_KEYS: &[&str] = &[
    "DOCKER_SANDBOXES",
    "SBX",
    "XDG_",
    "HOME",
    "USER",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "SSH_AUTH_SOCK",
];

/// Inspects the socket at `sock` and the daemon process serving it.
///
/// Returns a JSON report with the following keys:
///
/// - `socket`: path, socket type check, mode, owner UID and GID;
/// - `parents`: mode and ownership of each parent directory, innermost first;
/// - `daemon`: UID, GID and PID of the peer process, plus `cmdline`, `exe` and
///   either `environ_filtered` or `environ_error` when `/proc` allows it;
/// - `auditor_uid`: UID of the process running the audit;
/// - `questions`: the questions the auditor must answer from these facts.
///
/// # Errors
///
/// Fails if the socket path cannot be inspected, if connecting to the socket
/// fails, or if the peer credentials cannot be read. Unreadable `/proc` entries
/// or parent directories are not errors: the corresponding fields are omitted,
/// except for the environment, whose read error is reported as
/// `environ_error`.
pub(crate) async fn run(sock: &Path) -> Result<Value> {
    let meta = std::fs::symlink_metadata(sock)
        .with_context(|| format!("socket not found: {}", sock.display()))?;

    // Walk the parent directory chain: a 0755 parent owned by another user is
    // enough to make the socket reachable when the socket itself is permissive.
    let mut parents = vec![];
    for p in sock.ancestors().skip(1) {
        if p.as_os_str().is_empty() {
            break;
        }
        if let Ok(m) = std::fs::metadata(p) {
            parents.push(json!({
                "path": p.display().to_string(),
                "mode": format!("{:o}", m.mode() & 0o7777),
                "uid": m.uid(),
                "gid": m.gid(),
            }));
        }
        if p == Path::new("/") {
            break;
        }
    }

    // `SO_PEERCRED`: identity of the process that created the listening socket
    // (the daemon).
    let stream = UnixStream::connect(sock).await?;
    let cred = stream.peer_cred()?;
    let pid = cred.pid();

    let mut daemon = json!({ "uid": cred.uid(), "gid": cred.gid(), "pid": pid });
    if let Some(pid) = pid {
        let proc = format!("/proc/{pid}");
        if let Ok(cmd) = std::fs::read(format!("{proc}/cmdline")) {
            let cmd: Vec<String> = cmd
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).to_string())
                .collect();
            daemon["cmdline"] = json!(cmd);
        }
        if let Ok(exe) = std::fs::read_link(format!("{proc}/exe")) {
            daemon["exe"] = json!(exe.display().to_string());
        }
        // Only readable when running as the same user: whether this read
        // succeeds is already a finding in itself.
        match std::fs::read(format!("{proc}/environ")) {
            Ok(env) => {
                let vars: Vec<String> = env
                    .split(|b| *b == 0)
                    .map(|s| String::from_utf8_lossy(s).to_string())
                    .filter(|kv| ENV_KEYS.iter().any(|k| kv.starts_with(k)))
                    .collect();
                daemon["environ_filtered"] = json!(vars);
            }
            Err(e) => daemon["environ_error"] = json!(e.to_string()),
        }
    }

    // UID of the auditing process, to compare with the daemon and socket owners.
    let me = std::fs::metadata("/proc/self").map(|m| m.uid()).ok();

    Ok(json!({
        "socket": {
            "path": sock.display().to_string(),
            "is_socket": meta.file_type().is_socket(),
            "mode": format!("{:o}", meta.mode() & 0o7777),
            "uid": meta.uid(),
            "gid": meta.gid(),
        },
        "parents": parents,
        "daemon": daemon,
        "auditor_uid": me,
        "questions": [
            "Is the socket mode 0600 (or 0660 with a dedicated group)?",
            "Is any parent directory traversable or accessible by other accounts?",
            "Does the daemon run under the expected account (not root)?",
            "Which DOCKER_SANDBOXES_* variables influence the daemon?"
        ]
    }))
}
