//! SSH tunnel to a sandbox (`POST /sandbox/{name}/ssh`, `Upgrade: ssh`).
//!
//! The tunnel reaches the SSH server of the sandbox through the daemon socket,
//! so no network port is published.
//!
//! The socket is the only boundary: the SSH server of a sandbox authenticates
//! any user, `root` included, with the `none` method (measured on `sbx`
//! v0.54.0), so whoever holds a tunnel is `root` in the sandbox. A program
//! that relays a tunnel beyond the user who owns the daemon must authenticate
//! and authorize that relay itself.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::Sandbox;
use crate::error::Result;
use crate::transport::Request;

/// A byte stream connected to the SSH server of a sandbox.
///
/// The daemon starts by sending the SSH identification string
/// (`SSH-2.0-…`). Hand the tunnel to an SSH client library, or relay it to a
/// remote client, to open SSH sessions without exposing any network port.
///
/// # Security
///
/// The SSH server asks for no credential: any user name logs in, `root`
/// included. Never hand a tunnel to a client you would not give `root` in the
/// sandbox; a relay to remote clients needs authentication of its own.
pub struct SshTunnel {
    /// The upgraded connection.
    io: TokioIo<Upgraded>,
}

impl Sandbox {
    /// Opens an SSH tunnel to the sandbox (`POST /sandbox/{name}/ssh`).
    ///
    /// The tunnel opens, and the SSH server greets, even while the sandbox is
    /// stopped; opening it does not start the sandbox. Its holder can log in
    /// as any user, `root` included: see [`SshTunnel`].
    ///
    /// # Errors
    ///
    /// Fails if the daemon refuses to switch the connection to SSH.
    pub async fn ssh_tunnel(&self) -> Result<SshTunnel> {
        let request = Request::post("SSH tunnel", self.path("ssh"));
        let connection = self.transport().upgrade(request, "ssh").await?;
        Ok(SshTunnel { io: connection.io })
    }
}

impl AsyncRead for SshTunnel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl AsyncWrite for SshTunnel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

impl fmt::Debug for SshTunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshTunnel").finish_non_exhaustive()
    }
}
