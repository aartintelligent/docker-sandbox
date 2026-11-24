//! Incremental decoding of NDJSON response bodies.
//!
//! Sandbox creation, image pulls, the event stream and the approval prompts
//! answer with newline-delimited JSON that the daemon writes as things happen.
//! [`NdjsonStream`] decodes it line by line, as it arrives.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BytesMut;
use futures_core::Stream;
use http_body::Body;
use hyper::body::Incoming;

use crate::error::{Error, Result};

/// Decodes one NDJSON line. Returns `Ok(None)` for lines to skip.
pub(crate) type LineParser<T> = fn(&str) -> Result<Option<T>>;

/// A stream of items decoded from a newline-delimited JSON response, read as
/// the daemon produces it.
///
/// Implements [`Stream`]; the inherent [`NdjsonStream::next`] serves callers
/// that use no stream combinator library. Dropping the stream closes the
/// connection.
pub struct NdjsonStream<T> {
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
            operation,
            body,
            buffer: BytesMut::new(),
            parse,
            finished: false,
        }
    }

    /// Waits for the next item. Returns `None` once the daemon closes the
    /// stream.
    pub async fn next(&mut self) -> Option<Result<T>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }

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
        let this = self.get_mut();
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
        f.debug_struct("NdjsonStream")
            .field("operation", &self.operation)
            .field("buffered_bytes", &self.buffer.len())
            .field("finished", &self.finished)
            .finish_non_exhaustive()
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
