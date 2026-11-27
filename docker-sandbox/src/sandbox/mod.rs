//! Sandboxes: the [`Sandbox`] handle, the [`Sandboxes`] collection and the
//! payloads of their operations.
//!
//! [`Sandbox`] owns a clone of the [`Client`], so it can be stored and moved
//! into tasks. Its operations are spread over the module that owns their
//! payloads: life cycle here, then ports, credentials, sessions, files, SSH
//! and kit changes in the submodules, command execution in
//! [`exec`](crate::exec), MCP servers in [`mcp`](crate::mcp) and network
//! rules in [`policy`](crate::policy). Rustdoc shows them all on [`Sandbox`].

mod collection;
mod create;
mod credentials;
mod files;
mod info;
mod kits;
mod ports;
mod proxied_key;
mod session;
mod ssh;

use serde_json::json;

pub use self::collection::Sandboxes;
pub use self::create::{
    CreateEvent, CreateRequest, CreateStream, Created, PullPolicy, SkillsMode, Workspace,
};
pub use self::credentials::{
    CredentialSource, CredentialsOutcome, CredentialsUpdate, CustomSecret, Secret,
    ServiceCredential,
};
pub use self::info::{
    DeleteOptions, HostMount, RuntimeMount, SandboxInfo, SandboxState, SavedImage, StartOptions,
};
pub use self::kits::{SwapOutcome, SwapRequest};
pub use self::ports::PortBinding;
pub use self::proxied_key::ProxiedKey;
pub use self::session::SessionGuard;
pub use self::ssh::SshTunnel;
use crate::client::Client;
use crate::error::Result;
use crate::transport::{Request, Transport, segment};

/// Operations on one sandbox. Obtained with [`Client::sandbox`].
///
/// The handle only holds a name: no request is sent until an operation is
/// called, and cloning it is cheap.
///
/// ```no_run
/// use docker_sandbox::Client;
/// use docker_sandbox::exec::ExecSpec;
///
/// # async fn run() -> docker_sandbox::Result<()> {
/// let client = Client::connect().await?;
/// let sandbox = client.sandbox("job-42");
/// let _running = sandbox.ensure_running().await?;
/// sandbox.write_file("/tmp/input.txt", "42\n").await?;
/// let output = sandbox
///     .exec(&ExecSpec::new(["cat", "/tmp/input.txt"]))
///     .await?
///     .check()?;
/// assert_eq!(output.stdout, "42\n");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Sandbox {
    /// Client the requests go through.
    client: Client,
    /// Name of the sandbox.
    name: String,
}

impl Sandbox {
    /// The sandbox named `name`, reached through `client`.
    pub(crate) fn new(client: Client, name: String) -> Self {
        Self { client, name }
    }

    /// Name of the sandbox.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Client the handle sends its requests through.
    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Request transport.
    pub(crate) fn transport(&self) -> &Transport {
        self.client.transport()
    }

    /// `/sandbox/{name}/{action}`, or `/sandbox/{name}` for an empty action.
    pub(crate) fn path(&self, action: &str) -> String {
        sandbox_path(&self.name, action)
    }

    /// Current description (`GET /sandbox/{name}`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) if the sandbox
    /// does not exist.
    pub async fn inspect(&self) -> Result<SandboxInfo> {
        let request = Request::get("sandbox inspection", self.path(""));
        self.transport().json(request).await
    }

    /// Whether the sandbox exists.
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error other than
    /// not found.
    pub async fn exists(&self) -> Result<bool> {
        match self.inspect().await {
            Ok(_) => Ok(true),
            Err(error) if error.is_not_found() => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Starts the sandbox (`POST /sandbox/{name}/start`). Starting a running
    /// sandbox is a no-op.
    ///
    /// The daemon stops a sandbox about 30 seconds after its last session
    /// closes: hold a [`SessionGuard`], or use [`Sandbox::ensure_running`], to
    /// keep it running.
    ///
    /// # Errors
    ///
    /// Fails if the sandbox does not exist or cannot be started.
    pub async fn start(&self) -> Result<SandboxInfo> {
        self.start_with(&StartOptions::default()).await
    }

    /// Starts the sandbox with explicit options.
    ///
    /// # Errors
    ///
    /// Fails if the sandbox does not exist or cannot be started.
    pub async fn start_with(&self, options: &StartOptions) -> Result<SandboxInfo> {
        let request = Request::post("sandbox start", self.path("start"))
            .json(options)?
            .long_running();
        self.transport().json(request).await
    }

    /// Stops the sandbox (`POST /sandbox/{name}/stop`).
    ///
    /// # Errors
    ///
    /// Fails if the sandbox does not exist or cannot be stopped.
    pub async fn stop(&self) -> Result<SandboxInfo> {
        let request = Request::post("sandbox stop", self.path("stop"))
            .empty_json()
            .long_running();
        self.transport().json(request).await
    }

    /// Deletes the sandbox (`DELETE /sandbox/{name}`), refusing while it is in
    /// use.
    ///
    /// # Errors
    ///
    /// Fails if the sandbox does not exist or cannot be deleted.
    pub async fn delete(&self) -> Result<()> {
        self.delete_with(&DeleteOptions::default()).await
    }

    /// Deletes the sandbox, even while it is running or in use.
    ///
    /// # Errors
    ///
    /// Fails if the sandbox does not exist or cannot be deleted.
    pub async fn force_delete(&self) -> Result<()> {
        self.delete_with(&DeleteOptions::default().forced()).await
    }

    /// Deletes the sandbox with explicit options.
    ///
    /// The daemon requires the sandbox identifier as a guard against deleting
    /// a sandbox recreated under the same name; it is fetched first unless
    /// [`DeleteOptions::expected_id`] is set.
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) if the sandbox
    /// does not exist, and with a conflict one (`sandbox_identity_changed`)
    /// if it has another identifier.
    pub async fn delete_with(&self, options: &DeleteOptions) -> Result<()> {
        let id = match &options.expected_id {
            Some(id) => id.clone(),
            None => self.inspect().await?.id,
        };
        let request = Request::delete("sandbox deletion", self.path(""))
            .query("expected_id", id)
            .query("force", options.force.to_string())
            .long_running();
        self.transport().unit(request).await
    }

    /// Saves the sandbox as the image `tag`, usable as a template for new
    /// sandboxes (`POST /sandbox/{name}/save`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api)
    /// (`sandbox_not_found`) if the sandbox does not exist, and with the
    /// daemon error if it cannot be committed.
    pub async fn save(&self, tag: &str) -> Result<SavedImage> {
        let request = Request::post("sandbox save", self.path("save"))
            .json(&json!({ "tag": tag }))?
            .long_running();
        self.transport().json(request).await
    }

    /// Records the working directory of the sandbox
    /// (`PUT /sandbox/{name}/working-directory`), running or stopped.
    ///
    /// # Errors
    ///
    /// Fails if the sandbox does not exist or `dir` is empty.
    pub async fn set_working_dir(&self, dir: &str) -> Result<()> {
        let request = Request::put("working directory update", self.path("working-directory"))
            .json(&json!({ "working_dir": dir }))?;
        self.transport().unit(request).await
    }

    /// Grants the sandbox permission to use the host path `host_path`
    /// (`POST /sandbox/{name}/mounts`). This does not mount anything by
    /// itself; the grant shows in [`SandboxInfo::runtime_mounts`].
    ///
    /// # Errors
    ///
    /// Fails with a conflict [`Error::Api`](crate::Error::Api) if the sandbox
    /// is not running.
    pub async fn allow_host_path(&self, host_path: &str) -> Result<()> {
        let request = Request::post("host path grant", self.path("mounts"))
            .json(&json!({ "host_path": host_path }))?;
        self.transport().unit(request).await
    }

    /// Revokes a permission granted with [`Sandbox::allow_host_path`]
    /// (`DELETE /sandbox/{name}/mounts`).
    ///
    /// # Errors
    ///
    /// Fails with a conflict [`Error::Api`](crate::Error::Api) if the sandbox
    /// is not running.
    pub async fn revoke_host_path(&self, host_path: &str) -> Result<()> {
        let request = Request::delete("host path revocation", self.path("mounts"))
            .json(&json!({ "host_path": host_path }))?;
        self.transport().unit(request).await
    }

    /// Prometheus metrics of the sandbox VM
    /// (`GET /sandbox/{name}/debug/vm?route=/metrics`), in the text exposition
    /// format.
    ///
    /// # Errors
    ///
    /// Fails if the sandbox is not running.
    pub async fn vm_metrics(&self) -> Result<String> {
        let request = Request::get("VM metrics", self.path("debug/vm"))
            .query("route", "/metrics")
            .accept("*/*");
        let body = self.transport().bytes(request).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

/// `/sandbox/{name}/{action}`, or `/sandbox/{name}` for an empty action.
pub(crate) fn sandbox_path(name: &str, action: &str) -> String {
    if action.is_empty() {
        format!("/sandbox/{}", segment(name))
    } else {
        format!("/sandbox/{}/{action}", segment(name))
    }
}
