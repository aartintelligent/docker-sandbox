//! The connection to `sandboxd` and the entry point of every operation.
//!
//! A [`Client`] is connected once, with [`Client::connect`] or a
//! [`ClientBuilder`], then cloned freely: clones share the same configuration
//! and the handles it gives ([`Sandbox`], [`Daemon`], [`Images`], …) own a clone
//! of it, so that they can be stored in structures and moved into tasks.

mod builder;
mod socket;
mod version;

use std::path::Path;
use std::sync::Arc;

pub use self::builder::ClientBuilder;
pub use self::socket::{SOCKET_ENV, default_socket_path};
pub use self::version::{
    ApiVersion, InvalidApiVersion, SUPPORTED_API, VersionPolicy, VersionRange,
};
use crate::daemon::{Daemon, Health};
use crate::error::Result;
use crate::event::Events;
use crate::image::Images;
use crate::mcp::Mcp;
use crate::policy::Policy;
use crate::prompt::Prompts;
use crate::sandbox::{Sandbox, Sandboxes};
use crate::transport::Transport;

/// A client for one `sandboxd` daemon.
///
/// Cloning is cheap: clones share the same connection settings.
///
/// ```no_run
/// use docker_sandbox::Client;
///
/// # async fn run() -> docker_sandbox::Result<()> {
/// let client = Client::connect().await?;
/// println!("sandboxd {}", client.health().version);
/// for sandbox in client.sandboxes().list().await? {
///     println!("{} {}", sandbox.name, sandbox.status);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Client {
    /// Shared state of every clone.
    inner: Arc<Inner>,
}

/// State shared by the clones of a [`Client`].
#[derive(Debug)]
struct Inner {
    /// Request transport.
    transport: Transport,
    /// Health reported when the client connected.
    health: Health,
}

impl Client {
    /// Connects to the daemon of the current user with default settings.
    ///
    /// # Errors
    ///
    /// See [`ClientBuilder::connect`].
    pub async fn connect() -> Result<Self> {
        ClientBuilder::default().connect().await
    }

    /// Configures a client before connecting.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Path of the socket this client talks to.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        self.inner.transport.socket()
    }

    /// Health and version reported by the daemon when the client connected.
    #[must_use]
    pub fn health(&self) -> &Health {
        &self.inner.health
    }

    /// Daemon-wide operations: health, settings, log levels, disk usage.
    #[must_use]
    pub fn daemon(&self) -> Daemon {
        Daemon::new(self.clone())
    }

    /// Operations on the collection of sandboxes: listing and creation.
    #[must_use]
    pub fn sandboxes(&self) -> Sandboxes {
        Sandboxes::new(self.clone())
    }

    /// Operations on the sandbox named `name`.
    ///
    /// No request is sent until an operation is called, so the sandbox does
    /// not need to exist yet.
    #[must_use]
    pub fn sandbox(&self, name: impl Into<String>) -> Sandbox {
        Sandbox::new(self.clone(), name.into())
    }

    /// Operations on the images held by the daemon.
    #[must_use]
    pub fn images(&self) -> Images {
        Images::new(self.clone())
    }

    /// The daemon event stream.
    #[must_use]
    pub fn events(&self) -> Events {
        Events::new(self.clone())
    }

    /// Network policy: rules, checks, activity log.
    #[must_use]
    pub fn policy(&self) -> Policy {
        Policy::new(self.clone())
    }

    /// Daemon-wide MCP settings.
    #[must_use]
    pub fn mcp(&self) -> Mcp {
        Mcp::new(self.clone())
    }

    /// Approval prompts raised by the sandboxes.
    #[must_use]
    pub fn prompts(&self) -> Prompts {
        Prompts::new(self.clone())
    }

    /// Request transport, for the handles of the crate.
    pub(crate) fn transport(&self) -> &Transport {
        &self.inner.transport
    }
}
