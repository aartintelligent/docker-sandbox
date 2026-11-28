//! The collection of sandboxes: listing and creation.
//!
//! The daemon gives a creation up when its connection closes before the
//! result, as a dropped future closes it, but keeps the network rules of its
//! kits, which no route removes and which the next sandbox of that name gets
//! (`audit/assets/notes.md`, "Creation"). A creation is therefore read to its
//! end in a task of its own, and the sandbox it made is deleted, its rules
//! with it, when nobody took its result.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{mpsc, oneshot};

use super::{
    CreateEvent, CreateRequest, CreateStream, Created, DeleteOptions, SandboxInfo, sandbox_path,
};
use crate::client::Client;
use crate::error::{Error, Result};
use crate::transport::{NdjsonStream, Request};

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
    /// Dropping the future, on a timeout or a cancellation, does not give the
    /// creation up halfway, which would leave the network rules of its kits to
    /// the next sandbox of that name: the creation goes on to its end, then
    /// the sandbox it made is deleted, its rules with it. A sandbox of that
    /// name the request did not make (`409`) is never touched. A program that
    /// ends before the creation does still leaves those rules.
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
    /// [`CreateEvent::Result`] or [`CreateEvent::Error`]. A task of the
    /// library reads it to its end whatever the caller does: dropping the
    /// stream, or the future that returns it, before taking the result lets
    /// the creation finish, then deletes the sandbox it made, as
    /// [`Sandboxes::create`] does.
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
        let name = request.name.clone();
        let request = Request::post("sandbox creation", "/sandbox")
            .json(request)?
            .long_running();
        let (head, answered) = oneshot::channel();
        let (items, received) = mpsc::unbounded_channel();
        let taken = Arc::new(AtomicUsize::new(0));
        let stream = NdjsonStream::relayed(received, Arc::clone(&taken));
        tokio::spawn(relay_creation(Relay {
            client: self.client.clone(),
            name,
            request,
            head,
            items,
            taken,
        }));
        // Only a task that panicked, or a runtime shutting down, sends nothing.
        let gone = Error::Failed {
            operation: "sandbox creation",
            message: "the task of the creation ended without an answer".to_owned(),
        };
        answered.await.unwrap_or(Err(gone)).map(|()| stream)
    }
}

/// A creation read to its end by [`relay_creation`].
struct Relay {
    /// Client the requests go through.
    client: Client,
    /// Name of the sandbox to create.
    name: String,
    /// The creation request, ready to send.
    request: Request,
    /// Where the answer to the request goes: the stream begins, or an error.
    head: oneshot::Sender<Result<()>>,
    /// Where the items of the stream go, then `None` at its end.
    items: mpsc::UnboundedSender<Option<Result<CreateEvent>>>,
    /// How many items the reader took.
    taken: Arc<AtomicUsize>,
}

/// Sends the creation request of `relay` and relays its stream to its end.
/// Once the reader is gone, deletes the sandbox the creation made if the
/// reader did not take its result.
async fn relay_creation(relay: Relay) {
    let Relay {
        client,
        name,
        request,
        head,
        items,
        taken,
    } = relay;
    let mut stream = match client
        .transport()
        .ndjson(request, CreateEvent::parse_line)
        .await
    {
        Ok(stream) => stream,
        Err(error) => {
            let _ = head.send(Err(error));
            return;
        }
    };
    let _ = head.send(Ok(()));
    // The position of the result in the stream, and the sandbox it names.
    let mut created = None;
    let mut sent = 0;
    while let Some(item) = stream.next().await {
        if let Ok(CreateEvent::Result(result)) = &item {
            created = Some((sent, result.id.clone()));
        }
        sent += 1;
        let _ = items.send(Some(item));
    }
    let _ = items.send(None);
    let Some((position, id)) = created else {
        return;
    };
    // The reader takes the result, or gives it up with the stream.
    items.closed().await;
    if taken.load(Ordering::Acquire) <= position {
        tracing::info!(
            sandbox = name,
            "deleting the sandbox of a creation given up before its result"
        );
        let options = DeleteOptions::default().forced().with_expected_id(id);
        if let Err(error) = client.sandbox(&name).delete_with(&options).await {
            tracing::warn!(sandbox = name, %error, "cannot delete the sandbox of a creation given up");
        }
    }
}
