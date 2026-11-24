//! Running commands in a sandbox: to completion, interactively, or as a
//! session with the agent of the sandbox kit.
//!
//! - [`Sandbox::exec`] runs a command and returns its [`ExecOutput`];
//!   [`ExecOutput::check`] turns a non-zero exit code into an error.
//! - [`Sandbox::attach`] and [`Sandbox::agent_session`] return an
//!   [`ExecSession`]: write to its input, read its output chunk by chunk,
//!   resize its terminal and wait for its exit code.

mod output;
mod session;
mod spec;
mod stdcopy;

pub use self::output::{AttachOutput, ExecInspect, ExecOutput, OutputChunk, OutputStream};
pub use self::session::{ExecControl, ExecInput, ExecOutputStream, ExecSession};
pub use self::spec::{AgentSessionSpec, AttachSpec, ExecSpec, TerminalSize};
use crate::error::Result;
use crate::sandbox::Sandbox;
use crate::transport::{Request, segment};

impl Sandbox {
    /// Runs a command and waits for its outputs (`POST /sandbox/{name}/exec`).
    ///
    /// The deadline is [`ExecSpec::timeout`] when set, the long-operation
    /// timeout of the client otherwise.
    ///
    /// # Errors
    ///
    /// Fails with a conflict [`Error::Api`](crate::Error::Api) if the sandbox
    /// is stopped, with an invalid-input one if `cmd` is empty, and with
    /// [`Error::Timeout`](crate::Error::Timeout) when the deadline expires. A
    /// non-zero exit code is not an error, nor is a program that does not
    /// exist (exit code 127): check [`ExecOutput::exit_code`] or call
    /// [`ExecOutput::check`].
    pub async fn exec(&self, spec: &ExecSpec) -> Result<ExecOutput> {
        let request = Request::post("command execution", self.path("exec"))
            .json(spec)?
            .long_running_or(spec.timeout);
        self.transport().json(request).await
    }

    /// State of an execution started earlier
    /// (`GET /sandbox/{name}/exec/{exec_id}`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) if the
    /// execution is unknown.
    pub async fn exec_inspect(&self, exec_id: &str) -> Result<ExecInspect> {
        let path = format!("{}/{}", self.path("exec"), segment(exec_id));
        self.transport()
            .json(Request::get("execution state", path))
            .await
    }

    /// Starts an interactive command (`POST /sandbox/{name}/exec/attach`).
    ///
    /// # Errors
    ///
    /// Fails with a conflict [`Error::Api`](crate::Error::Api) if the sandbox
    /// is stopped, and with the daemon error if the command cannot be
    /// started.
    pub async fn attach(&self, spec: &AttachSpec) -> Result<ExecSession> {
        let request =
            Request::post("interactive execution", self.path("exec/attach")).json(spec)?;
        let connection = self.transport().upgrade(request, "tcp").await?;
        ExecSession::new(connection, self.clone(), spec.tty)
    }

    /// Starts a session with the agent of the sandbox kit
    /// (`POST /sandbox/{name}/agent/session`). The daemon starts a stopped
    /// sandbox for the session.
    ///
    /// # Errors
    ///
    /// Fails if the agent cannot be started.
    pub async fn agent_session(&self, spec: &AgentSessionSpec) -> Result<ExecSession> {
        let request = Request::post("agent session", self.path("agent/session")).json(spec)?;
        let connection = self.transport().upgrade(request, "tcp").await?;
        ExecSession::new(connection, self.clone(), spec.tty)
    }
}
