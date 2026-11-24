//! The daemon event stream (`GET /events`).
//!
//! On subscription the daemon first replays recent state, then sends an event
//! of kind [`EventKind::Sync`], then live events: sandboxes created, started,
//! stopped or deleted, published ports, network activity. The stream never
//! ends on its own; dropping it unsubscribes.

mod types;

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

pub use self::types::{DaemonEvent, EventKind};
use crate::client::Client;
use crate::error::Result;
use crate::transport::{NdjsonStream, Request};

/// Subscription to the daemon events. Obtained with
/// [`Client::events`](crate::Client::events).
#[derive(Debug, Clone)]
pub struct Events {
    /// Client the requests go through.
    client: Client,
}

impl Events {
    /// Event operations through `client`.
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Subscribes to every event, starting with the replay of recent state.
    ///
    /// The replay ends with an event of kind [`EventKind::Sync`]; use it to
    /// build an initial view of the sandboxes before following live events.
    ///
    /// # Errors
    ///
    /// Fails if the subscription cannot be opened. Errors while reading are
    /// reported as stream items.
    pub async fn subscribe(&self) -> Result<EventStream> {
        self.open(false).await
    }

    /// Subscribes to live events only: the replay and its
    /// [`EventKind::Sync`] marker are skipped.
    ///
    /// ```no_run
    /// use docker_sandbox::Client;
    /// use docker_sandbox::event::EventKind;
    ///
    /// # async fn run() -> docker_sandbox::Result<()> {
    /// let client = Client::connect().await?;
    /// let mut events = client.events().subscribe_live().await?;
    /// while let Some(event) = events.next().await {
    ///     let event = event?;
    ///     if event.kind() == EventKind::SandboxLifecycle {
    ///         println!("{:?} {}", event.sandbox_name, event.action);
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Fails if the subscription cannot be opened.
    pub async fn subscribe_live(&self) -> Result<EventStream> {
        self.open(true).await
    }

    /// Opens the stream, skipping the replay when `live_only` is set.
    async fn open(&self, live_only: bool) -> Result<EventStream> {
        let request = Request::get("event stream", "/events");
        let inner = self
            .client
            .transport()
            .ndjson(request, DaemonEvent::parse_line)
            .await?;
        Ok(EventStream {
            inner,
            replaying: live_only,
        })
    }
}

/// Stream of daemon events, returned by [`Events::subscribe`] and
/// [`Events::subscribe_live`].
///
/// Implements [`Stream`]; the inherent [`EventStream::next`] serves callers
/// that use no stream combinator library.
pub struct EventStream {
    /// Decoded NDJSON lines.
    inner: NdjsonStream<DaemonEvent>,
    /// Whether replayed events are still being skipped.
    replaying: bool,
}

impl EventStream {
    /// Waits for the next event. Returns `None` if the daemon closes the
    /// stream.
    pub async fn next(&mut self) -> Option<Result<DaemonEvent>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }
}

impl Stream for EventStream {
    type Item = Result<DaemonEvent>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(event))) if this.replaying => {
                    this.replaying = !event.is_sync();
                }
                other => return other,
            }
        }
    }
}

impl fmt::Debug for EventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream")
            .field("replaying", &self.replaying)
            .finish_non_exhaustive()
    }
}
