//! Incremental decoding of NDJSON response bodies.
//!
//! Sandbox creation, image pulls, the event stream and the approval prompts
//! answer with newline-delimited JSON that the daemon writes as things happen.
//! [`NdjsonStream`] decodes it line by line, as it arrives, or hands on the
//! items a task decodes from a body it reads to its end.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use bytes::BytesMut;
use futures_core::Stream;
use http_body::Body;
use hyper::body::Incoming;
use tokio::sync::mpsc;

use crate::error::{Error, Result};

/// Decodes one NDJSON line. Returns `Ok(None)` for lines to skip.
pub(crate) type LineParser<T> = fn(&str) -> Result<Option<T>>;

/// A stream of items decoded from a newline-delimited JSON response, read as
/// the daemon produces it.
///
/// Implements [`Stream`]; the inherent [`NdjsonStream::next`] serves callers
/// that use no stream combinator library. Dropping the stream closes the
/// connection, except for the stream of a sandbox creation, which the library
/// reads to its end
/// ([`Sandboxes::create_stream`](crate::sandbox::Sandboxes::create_stream)).
pub struct NdjsonStream<T> {
    /// Where its items come from.
    source: Source<T>,
}

/// Where the items of an [`NdjsonStream`] come from.
enum Source<T> {
    /// A response body, decoded as it arrives.
    Body(Decoder<T>),
    /// A task that decodes a body to its end, whatever the reader does.
    Relayed {
        /// The items, in the order of the body, then `None` at its end: the
        /// task keeps its sender until the reader is gone.
        items: mpsc::UnboundedReceiver<Option<Result<T>>>,
        /// How many items the reader took, which the task reads once the
        /// reader is gone.
        taken: Arc<AtomicUsize>,
        /// Whether the end was reached.
        ended: bool,
    },
}

/// The decoder of a newline-delimited JSON response body.
struct Decoder<T> {
    /// Operation the stream answers, used in errors.
    operation: &'static str,
    /// Response body.
    body: Incoming,
    /// Bytes received and not decoded yet.
    buffer: BytesMut,
    /// Decoder of one line.
    parse: LineParser<T>,
    /// Whether the body has ended.
    finished: bool,
}

impl<T> NdjsonStream<T> {
    /// Decodes `body` with `parse`.
    pub(crate) fn new(operation: &'static str, body: Incoming, parse: LineParser<T>) -> Self {
        Self {
            source: Source::Body(Decoder {
                operation,
                body,
                buffer: BytesMut::new(),
                parse,
                finished: false,
            }),
        }
    }

    /// Hands on the items a task sends on `items` until it sends `None`,
    /// counting in `taken` those the reader takes.
    pub(crate) fn relayed(
        items: mpsc::UnboundedReceiver<Option<Result<T>>>,
        taken: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            source: Source::Relayed {
                items,
                taken,
                ended: false,
            },
        }
    }

    /// Waits for the next item. Returns `None` once the daemon closes the
    /// stream.
    pub async fn next(&mut self) -> Option<Result<T>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }
}

impl<T> Decoder<T> {
    /// Decodes the next complete line already buffered, if any.
    fn next_buffered(&mut self) -> Option<Result<T>> {
        while let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
            let line = self.buffer.split_to(end + 1);
            match (self.parse)(&String::from_utf8_lossy(&line)) {
                Ok(Some(item)) => return Some(Ok(item)),
                Ok(None) => {}
                Err(error) => return Some(Err(error)),
            }
        }
        None
    }

    /// Decodes a trailing line left without a final newline.
    fn next_trailing(&mut self) -> Option<Result<T>> {
        if self.buffer.is_empty() {
            return None;
        }
        let rest = self.buffer.split();
        (self.parse)(&String::from_utf8_lossy(&rest)).transpose()
    }
}

impl<T> Stream for NdjsonStream<T> {
    type Item = Result<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut self.get_mut().source {
            Source::Body(decoder) => decoder.poll_item(cx),
            Source::Relayed {
                items,
                taken,
                ended,
            } => {
                if *ended {
                    return Poll::Ready(None);
                }
                match items.poll_recv(cx) {
                    Poll::Ready(Some(Some(item))) => {
                        taken.fetch_add(1, Ordering::AcqRel);
                        Poll::Ready(Some(item))
                    }
                    Poll::Ready(Some(None) | None) => {
                        *ended = true;
                        Poll::Ready(None)
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}

impl<T> Decoder<T> {
    /// Decodes the next item of the body, reading more of it when needed.
    fn poll_item(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T>>> {
        let this = self;
        loop {
            if let Some(item) = this.next_buffered() {
                return Poll::Ready(Some(item));
            }
            if this.finished {
                return Poll::Ready(this.next_trailing());
            }
            match Pin::new(&mut this.body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        this.buffer.extend_from_slice(&data);
                    }
                }
                Poll::Ready(Some(Err(source))) => {
                    this.finished = true;
                    this.buffer.clear();
                    return Poll::Ready(Some(Err(Error::Http {
                        operation: this.operation,
                        source,
                    })));
                }
                Poll::Ready(None) => this.finished = true,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<T> fmt::Debug for NdjsonStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Source::Body(decoder) => f
                .debug_struct("NdjsonStream")
                .field("operation", &decoder.operation)
                .field("buffered_bytes", &decoder.buffer.len())
                .field("finished", &decoder.finished)
                .finish_non_exhaustive(),
            Source::Relayed { taken, .. } => f
                .debug_struct("NdjsonStream")
                .field("relayed", &true)
                .field("taken", &taken.load(Ordering::Acquire))
                .finish_non_exhaustive(),
        }
    }
}

/// Decodes one line as JSON, skipping blank lines.
pub(crate) fn parse_json_line<T: serde::de::DeserializeOwned>(
    operation: &'static str,
    line: &str,
) -> Result<Option<T>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(line)
        .map(Some)
        .map_err(|source| Error::Decode { operation, source })
}
