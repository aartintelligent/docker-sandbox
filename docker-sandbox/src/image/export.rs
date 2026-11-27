//! The tar archive of an image, read as the daemon sends it.
//!
//! An image export can weigh hundreds of megabytes: [`ExportStream`] hands it
//! over chunk by chunk, so that a caller looking for a few files of it, as
//! kit forging does, never holds the whole archive in memory.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use futures_core::Stream;
use http_body::Body;
use hyper::body::Incoming;

use crate::error::{Error, Result};

/// The tar archive of an image, chunk by chunk, returned by
/// [`Images::export_stream`](super::Images::export_stream).
///
/// Implements [`Stream`]; the inherent [`ExportStream::next`] serves callers
/// that use no stream combinator library. A chunk may be empty. Dropping the
/// stream closes the connection.
pub struct ExportStream {
    /// Operation the stream answers, used in errors.
    operation: &'static str,
    /// Response body.
    body: Incoming,
}

impl ExportStream {
    /// Reads the archive from `body`.
    pub(crate) fn new(operation: &'static str, body: Incoming) -> Self {
        Self { operation, body }
    }

    /// Waits for the next chunk of the archive. Returns `None` once the
    /// archive has ended.
    pub async fn next(&mut self) -> Option<Result<Bytes>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }
}

impl Stream for ExportStream {
    type Item = Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let item = match ready!(Pin::new(&mut this.body).poll_frame(cx)) {
            // A trailers frame holds no data: it reads as an empty chunk.
            Some(Ok(frame)) => Some(Ok(frame.into_data().unwrap_or_default())),
            Some(Err(source)) => Some(Err(Error::Http {
                operation: this.operation,
                source,
            })),
            None => None,
        };
        Poll::Ready(item)
    }
}

impl fmt::Debug for ExportStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportStream")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}
