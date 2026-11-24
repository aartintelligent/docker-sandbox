//! The collection of sandboxes: listing and creation.

use super::{CreateEvent, CreateRequest, CreateStream, Created, SandboxInfo, sandbox_path};
use crate::client::Client;
use crate::error::{Error, Result};
use crate::transport::Request;

/// Operations on the collection of sandboxes. Obtained with
/// [`Client::sandboxes`].
#[derive(Debug, Clone)]
pub struct Sandboxes {
    /// Client the requests go through.
    client: Client,
}

impl Sandboxes {
    /// Collection operations through `client`.
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Every sandbox, running or stopped (`GET /sandbox`).
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn list(&self) -> Result<Vec<SandboxInfo>> {
        let request = Request::get("sandbox list", "/sandbox").query("include_stopped_at", "true");
        self.client.transport().json(request).await
    }

    /// One sandbox (`GET /sandbox/{name}`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`] if the sandbox does not exist.
    pub async fn get(&self, name: &str) -> Result<SandboxInfo> {
        let request = Request::get("sandbox inspection", sandbox_path(name, ""));
        self.client.transport().json(request).await
    }

    /// One sandbox, or `None` if it does not exist.
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error other than
    /// not found.
    pub async fn find(&self, name: &str) -> Result<Option<SandboxInfo>> {
        match self.get(name).await {
            Ok(sandbox) => Ok(Some(sandbox)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Creates a sandbox and waits for the outcome (`POST /sandbox`).
    ///
    /// The daemon streams progress and reports failures in-band with an HTTP
    /// 200 status; this method reads the whole stream and turns an `error`
    /// line into [`Error::Failed`]. Progress messages are logged at the debug
    /// level.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Failed`] when the daemon reports a failure, or with
    /// [`Error::Api`] when it rejects the request outright.
    pub async fn create(&self, request: &CreateRequest) -> Result<Created> {
        let mut stream = self.create_stream(request).await?;
        while let Some(event) = stream.next().await {
            match event? {
                CreateEvent::Result(created) => return Ok(*created),
                CreateEvent::Error(message) => {
                    return Err(Error::Failed {
                        operation: "sandbox creation",
                        message,
                    });
                }
                CreateEvent::Progress(message) => {
                    tracing::debug!(sandbox = request.name, %message, "sandbox creation progress");
                }
                CreateEvent::Unknown(_) => {}
            }
        }
        Err(Error::Failed {
            operation: "sandbox creation",
            message: "the progress stream ended without a result".to_owned(),
        })
    }

    /// Starts a sandbox creation and returns its progress as it happens
    /// (`POST /sandbox`).
    ///
    /// The stream yields [`CreateEvent::Progress`] items, then exactly one
    /// [`CreateEvent::Result`] or [`CreateEvent::Error`]. Dropping the stream
    /// does not cancel the creation on the daemon side.
    ///
    /// ```no_run
    /// use docker_sandbox::sandbox::{CreateEvent, CreateRequest, Created};
    ///
    /// # async fn run(client: docker_sandbox::Client, request: CreateRequest) -> docker_sandbox::Result<Option<Created>> {
    /// let mut stream = client.sandboxes().create_stream(&request).await?;
    /// while let Some(event) = stream.next().await {
    ///     match event? {
    ///         CreateEvent::Progress(line) => eprint!("{line}"),
    ///         // The result is boxed: `*created` moves it out.
    ///         CreateEvent::Result(created) => return Ok(Some(*created)),
    ///         CreateEvent::Error(message) => eprintln!("creation failed: {message}"),
    ///         _ => {}
    ///     }
    /// }
    /// # Ok(None)
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Api`] if the daemon rejects the request before
    /// streaming.
    pub async fn create_stream(&self, request: &CreateRequest) -> Result<CreateStream> {
        let request = Request::post("sandbox creation", "/sandbox")
            .json(request)?
            .long_running();
        self.client
            .transport()
            .ndjson(request, CreateEvent::parse_line)
            .await
    }
}
