//! Keeping a sandbox running.
//!
//! `sandboxd` stops a sandbox about 30 seconds after its last session closes.
//! A session is an open `GET /sandbox/{name}/session` request whose body never
//! ends; the `sbx` CLI holds one for the duration of every command. A program
//! that drives long-lived sandboxes holds its own with a [`SessionGuard`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::Incoming;
use tokio::task::JoinHandle;

use super::{CredentialsUpdate, Sandbox};
use crate::error::Result;
use crate::transport::{Request, Transport};

/// First delay before re-opening a session closed by the daemon.
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Longest delay between two attempts to re-open a session.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Keeps a session open on a sandbox for as long as it, or one of its clones,
/// is alive.
///
/// The session is re-opened automatically if the daemon closes it, until the
/// last clone is dropped or the sandbox no longer exists. Clones share the
/// same session, so one guard can be handed to every task that needs the
/// sandbox running. Holding a session does not start a stopped sandbox; see
/// [`Sandbox::ensure_running`].
#[derive(Debug, Clone)]
#[must_use = "the session is closed when the last guard is dropped"]
pub struct SessionGuard {
    /// State shared by the clones; the session closes when it is dropped.
    shared: Arc<Shared>,
}

/// State shared by the clones of a [`SessionGuard`].
#[derive(Debug)]
struct Shared {
    /// Name of the sandbox the session is held on.
    sandbox: String,
    /// Whether a session request is currently open.
    connected: Arc<AtomicBool>,
    /// Task that reads the session and re-opens it.
    task: JoinHandle<()>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SessionGuard {
    /// Opens a session and spawns the task that keeps it open.
    ///
    /// The first connection is established before returning, so that errors
    /// such as an unknown sandbox are reported to the caller.
    async fn open(transport: Transport, sandbox: String) -> Result<Self> {
        let path = session_path(&sandbox);
        let first = transport.stream(session_request(&path)).await?;
        let connected = Arc::new(AtomicBool::new(true));
        let task = tokio::spawn(keep_open(
            transport,
            path,
            first,
            Arc::clone(&connected),
            sandbox.clone(),
        ));
        Ok(Self {
            shared: Arc::new(Shared {
                sandbox,
                connected,
                task,
            }),
        })
    }

    /// Name of the sandbox the session is held on.
    #[must_use]
    pub fn sandbox(&self) -> &str {
        &self.shared.sandbox
    }

    /// Whether the session is currently open. It is briefly `false` while the
    /// guard re-opens a session closed by the daemon, and stays `false` once
    /// the sandbox no longer exists.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed) && !self.shared.task.is_finished()
    }

    /// Releases this guard. The session closes once every clone is released
    /// or dropped.
    pub fn release(self) {}
}

impl Sandbox {
    /// Holds a session on the sandbox, which keeps it running while the guard
    /// is alive (`GET /sandbox/{name}/session`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) if the sandbox
    /// does not exist.
    pub async fn hold_session(&self) -> Result<SessionGuard> {
        SessionGuard::open(self.transport().clone(), self.name().to_owned()).await
    }

    /// Makes sure the sandbox is running and stays so while the returned guard
    /// is alive.
    ///
    /// Follows the sequence of the `sbx` CLI: open a session, push an empty
    /// credentials update, then start the sandbox (a no-op when it already
    /// runs).
    ///
    /// # Errors
    ///
    /// Fails if the sandbox does not exist or cannot be started.
    pub async fn ensure_running(&self) -> Result<SessionGuard> {
        let session = self.hold_session().await?;
        self.push_credentials(&CredentialsUpdate::default()).await?;
        self.start().await?;
        Ok(session)
    }
}

/// `/sandbox/{name}/session`.
fn session_path(sandbox: &str) -> String {
    super::sandbox_path(sandbox, "session")
}

/// The request that opens a session at `path`.
fn session_request(path: &str) -> Request {
    Request::get("sandbox session", path)
}

/// Reads the session body until the daemon closes it, then re-opens it with
/// exponential backoff. Stops when the sandbox disappears.
async fn keep_open(
    transport: Transport,
    path: String,
    mut body: Incoming,
    connected: Arc<AtomicBool>,
    sandbox: String,
) {
    loop {
        while let Some(frame) = body.frame().await {
            if frame.is_err() {
                break;
            }
        }
        connected.store(false, Ordering::Relaxed);
        tracing::debug!(sandbox, "session closed by sandboxd, re-opening");

        let mut backoff = INITIAL_BACKOFF;
        body = loop {
            tokio::time::sleep(backoff).await;
            match transport.stream(session_request(&path)).await {
                Ok(body) => break body,
                Err(error) if error.is_not_found() => {
                    tracing::debug!(sandbox, "sandbox no longer exists, session ended");
                    return;
                }
                Err(error) => {
                    tracing::warn!(sandbox, %error, "cannot re-open session");
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        };
        connected.store(true, Ordering::Relaxed);
    }
}
