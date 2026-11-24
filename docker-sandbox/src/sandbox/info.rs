//! Description of a sandbox and the options of its life-cycle operations.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::PortBinding;
use crate::Extra;

/// A sandbox, as described by `GET /sandbox/{name}` and `GET /sandbox`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxInfo {
    /// Identifier assigned by the daemon. It guards deletion against a sandbox
    /// recreated under the same name.
    pub id: String,
    /// Sandbox name.
    pub name: String,
    /// Agent of the base kit, e.g. `shell` or `claude`.
    pub agent: String,
    /// Runtime state.
    pub status: SandboxState,
    /// Creation time (RFC 3339).
    #[serde(default)]
    pub created_at: Option<String>,
    /// Time of last use (RFC 3339).
    #[serde(default)]
    pub last_used_at: Option<String>,
    /// Host directory bind-mounted as workspace. Empty when there is none.
    #[serde(default)]
    pub workspace: String,
    /// Working directory recorded for the sandbox.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Working directory actually used inside the sandbox.
    #[serde(default)]
    pub effective_working_dir: Option<String>,
    /// Number of virtual CPUs, when set explicitly.
    #[serde(default)]
    pub cpus: Option<u32>,
    /// Memory limit, when set explicitly (`sbx` notation, e.g. `2g`).
    #[serde(default)]
    pub memory: Option<String>,
    /// Host directories mounted into the sandbox.
    #[serde(default)]
    pub host_mounts: Vec<HostMount>,
    /// Host paths granted at run time with
    /// [`Sandbox::allow_host_path`](super::Sandbox::allow_host_path).
    #[serde(default)]
    pub runtime_mounts: Vec<RuntimeMount>,
    /// Published ports. Only present in single-sandbox responses.
    #[serde(default)]
    pub ports: Vec<PortBinding>,
    /// Container labels set by `sbx`.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Service credentials configured for the sandbox, keyed by service.
    #[serde(default)]
    pub credential_sources: BTreeMap<String, Value>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl SandboxInfo {
    /// Whether the sandbox is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.status == SandboxState::Running
    }
}

/// Runtime state of a sandbox.
///
/// Unknown states are preserved in [`SandboxState::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
#[non_exhaustive]
pub enum SandboxState {
    /// The sandbox VM is running.
    Running,
    /// The sandbox exists but is not running.
    Stopped,
    /// Any other state reported by the daemon.
    Other(String),
}

impl SandboxState {
    /// Wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Other(s) => s,
        }
    }
}

impl From<String> for SandboxState {
    fn from(s: String) -> Self {
        match s.as_str() {
            "running" => Self::Running,
            "stopped" => Self::Stopped,
            _ => Self::Other(s),
        }
    }
}

impl From<SandboxState> for String {
    fn from(s: SandboxState) -> Self {
        s.as_str().to_owned()
    }
}

impl fmt::Display for SandboxState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// A host directory mounted into a sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMount {
    /// Directory on the host.
    pub host_path: String,
    /// Mount point inside the sandbox.
    #[serde(default)]
    pub container_target: Option<String>,
    /// Whether the mount is read-only.
    #[serde(default)]
    pub read_only: bool,
}

/// A host path granted to a running sandbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeMount {
    /// Directory on the host.
    pub host_path: String,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Options of [`Sandbox::start_with`](super::Sandbox::start_with)
/// (`POST /sandbox/{name}/start`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartOptions {
    /// Host SSH agent socket to forward into the sandbox. The daemon checks
    /// that it speaks the SSH agent protocol and keeps the previous socket
    /// otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_agent_socket_path: Option<String>,
}

impl StartOptions {
    /// Forwards the host SSH agent socket at `path`.
    #[must_use]
    pub fn with_ssh_agent_socket(mut self, path: impl Into<String>) -> Self {
        self.ssh_agent_socket_path = Some(path.into());
        self
    }
}

/// Options of [`Sandbox::delete_with`](super::Sandbox::delete_with)
/// (`DELETE /sandbox/{name}`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeleteOptions {
    /// Deletes the sandbox even while it is running or in use.
    pub force: bool,
    /// Identifier the sandbox must have. When unset, the current identifier
    /// is fetched first.
    pub expected_id: Option<String>,
}

impl DeleteOptions {
    /// Deletes the sandbox even while it is running or in use.
    #[must_use]
    pub fn forced(mut self) -> Self {
        self.force = true;
        self
    }

    /// Deletes the sandbox only if its identifier is `id`, the one returned at
    /// creation.
    #[must_use]
    pub fn with_expected_id(mut self, id: impl Into<String>) -> Self {
        self.expected_id = Some(id.into());
        self
    }
}

/// Image a sandbox was saved to, returned by
/// [`Sandbox::save`](super::Sandbox::save).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedImage {
    /// Image identifier (`sha256:…`).
    pub image_id: String,
    /// Fully qualified tag, e.g. `docker.io/library/snap:v1`.
    pub tag: String,
}
