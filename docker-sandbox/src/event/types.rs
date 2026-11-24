//! Payloads of the event stream.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Extra;
use crate::error::Result;
use crate::transport::parse_json_line;

/// An event published by the daemon.
///
/// On subscription, the daemon first replays recent state, then sends an event
/// of type `sync` (action `complete`), then live events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonEvent {
    /// Unique event identifier.
    pub id: String,
    /// Event family, e.g. `sandbox.lifecycle`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// What happened, e.g. `created`, `started`, `published`.
    pub action: String,
    /// Emission time (RFC 3339).
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Sandbox the event refers to, if any.
    #[serde(default)]
    pub sandbox_name: Option<String>,
    /// Identifier of that sandbox, if any.
    #[serde(default)]
    pub sandbox_id: Option<String>,
    /// Per-sandbox sequence number, if any.
    #[serde(default)]
    pub seq: Option<u64>,
    /// Event-specific payload.
    #[serde(default)]
    pub data: Option<Value>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Event families observed on `sbx` v0.54.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EventKind {
    /// End of the initial replay (`sync` / `complete`).
    Sync,
    /// Sandbox created, started, stopped or deleted (`sandbox.lifecycle`).
    SandboxLifecycle,
    /// Published ports changed (`sandbox.ports`).
    SandboxPorts,
    /// Outbound network activity seen by the proxy (`policy.network`).
    PolicyNetwork,
    /// Any other family.
    Other,
}

impl DaemonEvent {
    /// Family of the event.
    #[must_use]
    pub fn kind(&self) -> EventKind {
        match self.event_type.as_str() {
            "sync" => EventKind::Sync,
            "sandbox.lifecycle" => EventKind::SandboxLifecycle,
            "sandbox.ports" => EventKind::SandboxPorts,
            "policy.network" => EventKind::PolicyNetwork,
            _ => EventKind::Other,
        }
    }

    /// Whether this event marks the end of the initial replay.
    #[must_use]
    pub fn is_sync(&self) -> bool {
        self.kind() == EventKind::Sync
    }

    /// Whether the event refers to the sandbox named `name`.
    #[must_use]
    pub fn is_about(&self, name: &str) -> bool {
        self.sandbox_name.as_deref() == Some(name)
    }

    /// Parses one NDJSON line of the event stream.
    pub(crate) fn parse_line(line: &str) -> Result<Option<Self>> {
        parse_json_line("event stream", line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_classified() {
        let line = r#"{"id":"1","type":"sandbox.lifecycle","action":"created","sandbox_name":"job-42","seq":1,"new":true}"#;
        let event = DaemonEvent::parse_line(line).unwrap().unwrap();
        assert_eq!(event.kind(), EventKind::SandboxLifecycle);
        assert!(event.is_about("job-42"));
        assert_eq!(event.extra["new"], true);

        let sync = DaemonEvent::parse_line(r#"{"id":"2","type":"sync","action":"complete"}"#)
            .unwrap()
            .unwrap();
        assert!(sync.is_sync());
        assert!(DaemonEvent::parse_line("  \n").unwrap().is_none());
    }
}
