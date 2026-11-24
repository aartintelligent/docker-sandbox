//! Interactive executions over upgraded connections.
//!
//! `POST /sandbox/{name}/exec/attach` and `POST /sandbox/{name}/agent/session`
//! switch the connection to a raw byte stream (`Upgrade: tcp`): bytes written
//! by the client go to the standard input of the process, and shutting down
//! the write side closes it. The execution identifier comes back in the
//! `Sandboxes-Exec-Id` response header; it is used to resize the terminal and
//! to read the exit code.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_core::Stream;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf};

use super::stdcopy::StdcopyDecoder;
use super::{AttachOutput, ExecInspect, OutputChunk, OutputStream, TerminalSize};
use crate::error::{Error, Result};
use crate::sandbox::Sandbox;
use crate::transport::{Request, UpgradedConnection, segment};

/// Response header carrying the execution identifier.
const EXEC_ID_HEADER: &str = "sandboxes-exec-id";

/// First delay between two state checks in [`ExecControl::wait`].
const FIRST_POLL: Duration = Duration::from_millis(50);

/// Longest delay between two state checks in [`ExecControl::wait`].
const MAX_POLL: Duration = Duration::from_secs(1);

/// An interactive execution in a sandbox.
///
/// Use [`ExecSession::collect`] to run a command to completion, or
/// [`ExecSession::split`] to drive its input and output concurrently.
pub struct ExecSession {
    /// The upgraded connection.
    io: TokioIo<Upgraded>,
    /// Whether a terminal was allocated, which makes the output raw.
    tty: bool,
    /// Resizing and exit code.
    control: ExecControl,
}

impl ExecSession {
    /// Wraps the upgraded `connection` of an execution started in `sandbox`.
    pub(crate) fn new(connection: UpgradedConnection, sandbox: Sandbox, tty: bool) -> Result<Self> {
        let id = connection
            .headers
            .get(EXEC_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                Error::InvalidRequest(
                    "sandboxd did not return the Sandboxes-Exec-Id header".to_owned(),
                )
            })?
            .to_owned();
        Ok(Self {
            io: connection.io,
            tty,
            control: ExecControl { sandbox, id },
        })
    }

    /// Execution identifier assigned by the daemon.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.control.id
    }

    /// A handle to resize the terminal and read the exit code.
    #[must_use]
    pub fn control(&self) -> ExecControl {
        self.control.clone()
    }

    /// Splits the session into its input, its output and a control handle.
    #[must_use]
    pub fn split(self) -> (ExecInput, ExecOutputStream, ExecControl) {
        let (reader, writer) = tokio::io::split(self.io);
        let output = ExecOutputStream {
            reader,
            decoder: StdcopyDecoder::new(self.tty),
            finished: false,
        };
        (ExecInput { writer }, output, self.control)
    }

    /// Writes `stdin`, closes the input, reads the whole output and waits for
    /// the exit code (at most `exit_timeout` after the output ends).
    ///
    /// # Errors
    ///
    /// Fails if the connection breaks or the exit code is not available in
    /// time.
    pub async fn collect(
        self,
        stdin: impl Into<Bytes>,
        exit_timeout: Duration,
    ) -> Result<AttachOutput> {
        let (mut input, mut output, control) = self.split();
        let stdin = stdin.into();
        let writer = async move {
            if !stdin.is_empty() {
                input.write_all(&stdin).await?;
            }
            input.shutdown().await
        };
        let reader = async {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            while let Some(chunk) = output.next().await {
                let chunk = chunk?;
                match chunk.stream {
                    OutputStream::Stdout => stdout.extend_from_slice(&chunk.data),
                    OutputStream::Stderr => stderr.extend_from_slice(&chunk.data),
                }
            }
            Ok::<_, Error>((stdout, stderr))
        };
        let (written, read) = tokio::join!(writer, reader);
        let (stdout, stderr) = read?;
        // A process that exits without reading its input closes the stream
        // first: that is not a failure of the execution.
        if let Err(error) = written {
            tracing::debug!(%error, "standard input closed before all data was written");
        }
        let exit_code = control.wait(exit_timeout).await?;
        Ok(AttachOutput {
            stdout,
            stderr,
            exit_code,
        })
    }
}

impl fmt::Debug for ExecSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecSession")
            .field("id", &self.control.id)
            .field("tty", &self.tty)
            .finish_non_exhaustive()
    }
}

/// Standard input of an interactive execution.
///
/// Shutting it down ([`AsyncWriteExt::shutdown`]) closes the process input.
pub struct ExecInput {
    /// Write half of the upgraded connection.
    writer: WriteHalf<TokioIo<Upgraded>>,
}

impl AsyncWrite for ExecInput {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

impl fmt::Debug for ExecInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecInput").finish_non_exhaustive()
    }
}

/// Output of an interactive execution, as a stream of [`OutputChunk`]s.
pub struct ExecOutputStream {
    /// Read half of the upgraded connection.
    reader: ReadHalf<TokioIo<Upgraded>>,
    /// Decoder of the framing.
    decoder: StdcopyDecoder,
    /// Whether the connection has ended.
    finished: bool,
}

impl ExecOutputStream {
    /// Waits for the next chunk. Returns `None` once the process output ends.
    pub async fn next(&mut self) -> Option<Result<OutputChunk>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }
}

impl Stream for ExecOutputStream {
    type Item = Result<OutputChunk>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let mut buffer = [0_u8; 8192];
        loop {
            if let Some(chunk) = this.decoder.next_chunk() {
                return Poll::Ready(Some(Ok(chunk)));
            }
            if this.finished {
                return Poll::Ready(this.decoder.finish().map(Ok));
            }
            let mut read = ReadBuf::new(&mut buffer);
            match Pin::new(&mut this.reader).poll_read(cx, &mut read) {
                Poll::Ready(Ok(())) if read.filled().is_empty() => this.finished = true,
                Poll::Ready(Ok(())) => this.decoder.feed(read.filled()),
                Poll::Ready(Err(error)) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(Error::Connection(error))));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl fmt::Debug for ExecOutputStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecOutputStream")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

/// Control handle of an interactive execution: terminal resizing and exit
/// code.
///
/// Cheap to clone and independent of the connection.
#[derive(Debug, Clone)]
pub struct ExecControl {
    /// Sandbox the execution runs in.
    sandbox: Sandbox,
    /// Execution identifier.
    id: String,
}

impl ExecControl {
    /// Execution identifier assigned by the daemon.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Resizes the terminal (`POST /sandbox/{name}/exec/{exec_id}/resize`).
    ///
    /// # Errors
    ///
    /// Fails with a conflict [`Error::Api`] (`exec_not_running`) once the
    /// process has exited, and with a not-found one for an unknown execution.
    pub async fn resize(&self, size: TerminalSize) -> Result<()> {
        let path = format!("{}/{}/resize", self.sandbox.path("exec"), segment(&self.id));
        let request = Request::post("terminal resize", path)
            .query("h", size.height.to_string())
            .query("w", size.width.to_string());
        self.sandbox.transport().unit(request).await
    }

    /// Current state of the execution (`GET /sandbox/{name}/exec/{exec_id}`).
    ///
    /// # Errors
    ///
    /// Fails if the execution is unknown.
    pub async fn inspect(&self) -> Result<ExecInspect> {
        self.sandbox.exec_inspect(&self.id).await
    }

    /// Waits until the process has exited and returns its exit code.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Timeout`] if the process is still running after
    /// `timeout`.
    pub async fn wait(&self, timeout: Duration) -> Result<i32> {
        let deadline = Instant::now() + timeout;
        let mut delay = FIRST_POLL;
        loop {
            let state = self.inspect().await?;
            if !state.running {
                return Ok(state.exit_code.unwrap_or(-1));
            }
            if Instant::now() + delay > deadline {
                return Err(Error::Timeout {
                    operation: "execution exit",
                    after: timeout,
                });
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(MAX_POLL);
        }
    }
}
