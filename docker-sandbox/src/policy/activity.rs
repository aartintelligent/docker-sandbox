//! What the policy decides for a target, and what the sandboxes reached.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Extra;

/// A target to check against the policy, sent to
/// `POST /policy/network/check`.
///
/// ```
/// use docker_sandbox::policy::NetworkCheck;
///
/// let host = NetworkCheck::host("api.anthropic.com");
/// let request = NetworkCheck::http("POST", "https://api.anthropic.com/v1/messages");
/// # let _ = (host, request);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkCheck {
    /// Kind of target.
    #[serde(rename = "type")]
    pub kind: CheckKind,
    /// Host, `host:port` or URL.
    pub target: String,
    /// HTTP method of an HTTP check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

impl NetworkCheck {
    /// Whether a connection to `target` (a host or `host:port`) is allowed.
    pub fn host(target: impl Into<String>) -> Self {
        Self {
            kind: CheckKind::Network,
            target: target.into(),
            method: None,
        }
    }

    /// Whether an HTTP request `method url` is allowed.
    pub fn http(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            kind: CheckKind::Http,
            target: url.into(),
            method: Some(method.into()),
        }
    }
}

/// Kind of a [`NetworkCheck`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CheckKind {
    /// A connection to a host.
    Network,
    /// An HTTP request.
    Http,
}

/// Decision of the policy for a [`NetworkCheck`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    /// Whether the target is allowed.
    pub allowed: bool,
    /// Rule that decided.
    #[serde(default)]
    pub rule: Option<String>,
    /// Action checked, e.g. `net:connect:tcp`.
    #[serde(default)]
    pub action: Option<String>,
    /// Kind of resource matched, e.g. `net:domain`.
    #[serde(default)]
    pub resource_type: Option<String>,
    /// Resource matched, e.g. `api.anthropic.com:443`.
    #[serde(default)]
    pub resource_value: Option<String>,
    /// Normalized target.
    #[serde(default)]
    pub target: Option<String>,
    /// Scope of the decision, e.g. `global`.
    #[serde(default)]
    pub context: Option<String>,
    /// Organization governance state.
    #[serde(default)]
    pub governance: Option<Value>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Hosts the sandboxes reached or were refused, from
/// `GET /policy/network/log`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkLog {
    /// Hosts that were allowed.
    #[serde(default)]
    pub allowed_hosts: Vec<HostActivity>,
    /// Hosts that were blocked.
    #[serde(default)]
    pub blocked_hosts: Vec<HostActivity>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl NetworkLog {
    /// Allowed and blocked activity of the sandbox named `name`.
    #[must_use]
    pub fn for_sandbox(&self, name: &str) -> Self {
        let keep = |hosts: &[HostActivity]| {
            hosts
                .iter()
                .filter(|h| h.vm_name.as_deref() == Some(name))
                .cloned()
                .collect()
        };
        Self {
            allowed_hosts: keep(&self.allowed_hosts),
            blocked_hosts: keep(&self.blocked_hosts),
            extra: Extra::new(),
        }
    }
}

/// Traffic of one sandbox to one host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostActivity {
    /// Host and port, e.g. `api.anthropic.com:443`.
    pub host: String,
    /// Sandbox the traffic came from.
    #[serde(default)]
    pub vm_name: Option<String>,
    /// Number of connections since [`HostActivity::since`].
    #[serde(default)]
    pub count_since: u64,
    /// Start of the count (RFC 3339).
    #[serde(default)]
    pub since: Option<String>,
    /// Last connection (RFC 3339).
    #[serde(default)]
    pub last_seen: Option<String>,
    /// How the proxy handled the traffic, e.g. `forward`.
    #[serde(default)]
    pub proxy_type: Option<String>,
    /// Rule that decided, when one did.
    #[serde(default)]
    pub rule: Option<String>,
    /// Why the traffic was blocked, for a blocked host.
    #[serde(default)]
    pub reason: Option<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}
