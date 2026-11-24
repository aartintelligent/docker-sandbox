//! Descriptions of the commands to run: synchronous, interactive, or the agent
//! of the sandbox kit.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A command to run to completion inside a sandbox
/// (`POST /sandbox/{name}/exec`).
///
/// ```
/// use std::time::Duration;
/// use docker_sandbox::exec::ExecSpec;
///
/// let spec = ExecSpec::new(["make", "test"])
///     .with_user("0")
///     .with_working_dir("/work")
///     .with_env("CI", "true")
///     .with_timeout(Duration::from_secs(600));
/// assert_eq!(spec.cmd, ["make", "test"]);
///
/// let script = ExecSpec::shell("cd /work && make test");
/// assert_eq!(script.cmd, ["sh", "-c", "cd /work && make test"]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecSpec {
    /// Program and arguments. Not interpreted by a shell.
    pub cmd: Vec<String>,
    /// User name or UID (`"0"` for root). Defaults to the kit's user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Working directory. Defaults to the kit's working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Additional environment variables.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Client-side deadline of the execution. Defaults to the long-operation
    /// timeout of the client. Not sent to the daemon: the process keeps
    /// running in the sandbox when the deadline expires.
    #[serde(skip)]
    pub timeout: Option<Duration>,
}

impl ExecSpec {
    /// A command with default user, directory and environment.
    pub fn new(cmd: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            cmd: cmd.into_iter().map(Into::into).collect(),
            user: None,
            working_dir: None,
            env: BTreeMap::new(),
            timeout: None,
        }
    }

    /// A shell script, run with `sh -c`.
    pub fn shell(script: impl Into<String>) -> Self {
        Self::new(["sh".to_owned(), "-c".to_owned(), script.into()])
    }

    /// Runs the command as `user` (name or UID).
    #[must_use]
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Runs the command in `dir`.
    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Adds an environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Adds environment variables.
    #[must_use]
    pub fn with_envs<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.env
            .extend(vars.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Gives up waiting after `timeout`, with [`Error::Timeout`](crate::Error::Timeout).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// Terminal dimensions, in characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TerminalSize {
    /// Number of rows.
    pub height: u16,
    /// Number of columns.
    pub width: u16,
}

impl TerminalSize {
    /// A terminal of `height` rows and `width` columns.
    #[must_use]
    pub const fn new(height: u16, width: u16) -> Self {
        Self { height, width }
    }
}

impl Default for TerminalSize {
    /// 24 rows by 80 columns.
    fn default() -> Self {
        Self::new(24, 80)
    }
}

/// An interactive command (`POST /sandbox/{name}/exec/attach`).
///
/// ```
/// use docker_sandbox::exec::{AttachSpec, ExecSpec, TerminalSize};
///
/// let spec = AttachSpec::new(["bash"]).with_tty(TerminalSize::new(40, 120));
/// assert!(spec.tty);
///
/// let piped = AttachSpec::from(ExecSpec::new(["wc", "-l"]).with_user("0"));
/// assert_eq!(piped.user.as_deref(), Some("0"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachSpec {
    /// Program and arguments. Not interpreted by a shell.
    pub cmd: Vec<String>,
    /// User name or UID (`"0"` for root). Defaults to the kit's user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Working directory. Defaults to the kit's working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Additional environment variables.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Allocates a pseudo-terminal. Output is then a single raw stream.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tty: bool,
    /// Initial terminal size, when `tty` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_size: Option<TerminalSize>,
}

impl AttachSpec {
    /// An interactive command without a terminal.
    pub fn new(cmd: impl IntoIterator<Item = impl Into<String>>) -> Self {
        ExecSpec::new(cmd).into()
    }

    /// An interactive shell script, run with `sh -c`.
    pub fn shell(script: impl Into<String>) -> Self {
        ExecSpec::shell(script).into()
    }

    /// Runs the command as `user` (name or UID).
    #[must_use]
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Runs the command in `dir`.
    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Adds an environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Adds environment variables.
    #[must_use]
    pub fn with_envs<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.env
            .extend(vars.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Allocates a pseudo-terminal of the given size.
    #[must_use]
    pub fn with_tty(mut self, size: TerminalSize) -> Self {
        self.tty = true;
        self.terminal_size = Some(size);
        self
    }
}

impl From<ExecSpec> for AttachSpec {
    /// The same command, without a terminal. The client-side timeout of
    /// `spec` does not apply to an interactive command.
    fn from(spec: ExecSpec) -> Self {
        Self {
            cmd: spec.cmd,
            user: spec.user,
            working_dir: spec.working_dir,
            env: spec.env,
            tty: false,
            terminal_size: None,
        }
    }
}

/// A session with the agent of the sandbox kit
/// (`POST /sandbox/{name}/agent/session`).
///
/// ```
/// use docker_sandbox::exec::{AgentSessionSpec, TerminalSize};
///
/// let spec = AgentSessionSpec::default()
///     .with_tty(TerminalSize::new(40, 120))
///     .with_working_dir("/home/agent/repo");
/// assert_eq!(spec.terminal_size, Some(TerminalSize::new(40, 120)));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionSpec {
    /// Arguments of the agent, as `sbx run AGENT -- ARGS` passes them: when
    /// the first starts with `-`, they follow the default command of the kit;
    /// otherwise they replace it, the entrypoint of the image kept. The daemon
    /// applies the rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Allocates a pseudo-terminal. Agents generally expect one.
    #[serde(default)]
    pub tty: bool,
    /// Initial terminal size, when `tty` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_size: Option<TerminalSize>,
    /// Working directory. Defaults to the sandbox working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Additional environment variables.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Host SSH agent socket to forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_agent_socket_path: Option<String>,
}

impl Default for AgentSessionSpec {
    /// A terminal session of 24 rows by 80 columns.
    fn default() -> Self {
        Self {
            args: Vec::new(),
            tty: true,
            terminal_size: Some(TerminalSize::default()),
            working_dir: None,
            env: BTreeMap::new(),
            ssh_agent_socket_path: None,
        }
    }
}

impl AgentSessionSpec {
    /// Passes `args` to the agent: options added to its default command
    /// (`["--continue"]`), or a prompt in its place (`["fix the build"]`).
    ///
    /// ```
    /// use docker_sandbox::exec::AgentSessionSpec;
    ///
    /// let spec = AgentSessionSpec::default().with_args(["--model", "sonnet"]);
    /// assert_eq!(spec.args, ["--model", "sonnet"]);
    /// ```
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Uses a pseudo-terminal of the given size.
    #[must_use]
    pub fn with_tty(mut self, size: TerminalSize) -> Self {
        self.tty = true;
        self.terminal_size = Some(size);
        self
    }

    /// Runs the agent without a terminal.
    #[must_use]
    pub fn without_tty(mut self) -> Self {
        self.tty = false;
        self.terminal_size = None;
        self
    }

    /// Runs the agent in `dir`.
    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Adds an environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Adds environment variables.
    #[must_use]
    pub fn with_envs<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.env
            .extend(vars.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Forwards the host SSH agent socket at `path`.
    #[must_use]
    pub fn with_ssh_agent_socket(mut self, path: impl Into<String>) -> Self {
        self.ssh_agent_socket_path = Some(path.into());
        self
    }
}
