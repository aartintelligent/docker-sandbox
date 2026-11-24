//! Configuration of a [`Client`] before it connects.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::version::{self, VersionPolicy};
use super::{Client, Inner, default_socket_path};
use crate::daemon::Health;
use crate::error::{Error, Result};
use crate::transport::{Request, Transport};

/// Default deadline of regular requests.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default deadline of long operations.
const DEFAULT_LONG_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Configures and connects a [`Client`]. Obtained with [`Client::builder`].
///
/// ```no_run
/// use std::time::Duration;
/// use docker_sandbox::{Client, VersionPolicy};
///
/// # async fn run() -> docker_sandbox::Result<()> {
/// let client = Client::builder()
///     .with_socket("/run/user/1000/sandboxd.sock")
///     .with_timeout(Duration::from_secs(10))
///     .with_version_policy(VersionPolicy::Warn)
///     .connect()
///     .await?;
/// # let _ = client;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
#[must_use = "a builder does nothing until `connect` is called"]
pub struct ClientBuilder {
    /// Explicit socket path, instead of [`default_socket_path`].
    socket: Option<PathBuf>,
    /// Deadline of regular requests.
    timeout: Duration,
    /// Deadline of long operations.
    long_timeout: Duration,
    /// Behavior outside [`SUPPORTED_API`](crate::SUPPORTED_API).
    version_policy: VersionPolicy,
}

impl Default for ClientBuilder {
    /// Default socket path, 30 s per request, 15 min for long operations,
    /// strict version check.
    fn default() -> Self {
        Self {
            socket: None,
            timeout: DEFAULT_TIMEOUT,
            long_timeout: DEFAULT_LONG_TIMEOUT,
            version_policy: VersionPolicy::Strict,
        }
    }
}

impl ClientBuilder {
    /// Uses an explicit socket path instead of [`default_socket_path`].
    pub fn with_socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket = Some(path.into());
        self
    }

    /// Deadline of regular requests (default: 30 seconds).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Deadline of long operations (default: 15 minutes): sandbox creation,
    /// start and stop, command execution, file and image transfers.
    ///
    /// A command can set its own deadline with
    /// [`ExecSpec::with_timeout`](crate::exec::ExecSpec::with_timeout).
    pub fn with_long_operation_timeout(mut self, timeout: Duration) -> Self {
        self.long_timeout = timeout;
        self
    }

    /// Behavior when the daemon API version is not in [`SUPPORTED_API`](crate::SUPPORTED_API)
    /// (default: [`VersionPolicy::Strict`]).
    pub fn with_version_policy(mut self, policy: VersionPolicy) -> Self {
        self.version_policy = policy;
        self
    }

    /// Connects to the daemon and checks its API version.
    ///
    /// # Errors
    ///
    /// - [`Error::NoSocketPath`] if no socket path is given and none can be
    ///   derived from the environment;
    /// - [`Error::Connect`] if the socket cannot be reached;
    /// - [`Error::UnsupportedVersion`] if the API version is outside
    ///   [`SUPPORTED_API`](crate::SUPPORTED_API) and the policy is [`VersionPolicy::Strict`].
    pub async fn connect(self) -> Result<Client> {
        let socket = self
            .socket
            .or_else(default_socket_path)
            .ok_or(Error::NoSocketPath)?;
        let transport = Transport::new(socket, self.timeout, self.long_timeout);
        let health: Health = transport
            .json(Request::get("daemon health", "/daemon/health"))
            .await?;
        version::check(&health.api_version, self.version_policy)?;
        Ok(Client {
            inner: Arc::new(Inner { transport, health }),
        })
    }
}
