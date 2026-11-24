//! Daemon-level payloads: health, information, settings, disk usage.
//!
//! Field names follow the JSON of the daemon. Unknown fields are kept in the
//! `extra` maps, so that a newer daemon does not break decoding.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Extra;

/// Response of `GET /daemon/health`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    /// Overall status, `healthy` when the daemon is operational.
    pub status: String,
    /// Release version, e.g. `v0.54.0`.
    pub version: String,
    /// Version of the daemon API, e.g. `0.52.0`.
    pub api_version: String,
    /// Source revision of the release.
    #[serde(default)]
    pub revision: Option<String>,
    /// Whether this is a release build.
    #[serde(default)]
    pub release: Option<bool>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Health {
    /// Whether the daemon reports itself as healthy.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.status == "healthy"
    }
}

/// Response of `GET /daemon/info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// Path of the API socket served by the daemon.
    #[serde(default)]
    pub api_socket: Option<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A daemon setting, as returned by `GET /daemon/settings/{key}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Setting {
    /// Setting key, e.g. `sandbox.disk.dockerVolume`.
    pub key: String,
    /// Effective value.
    pub value: Value,
    /// Default value.
    #[serde(default)]
    pub default: Value,
    /// Where the effective value comes from: `default`, `override`, `remote`, …
    #[serde(default)]
    pub source: Option<String>,
    /// Value type: `bool`, `string`, `int`, …
    #[serde(rename = "type", default)]
    pub value_type: Option<String>,
    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Response wrapper of `GET /daemon/settings`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SettingsResponse {
    /// Every setting.
    pub(crate) settings: Vec<Setting>,
}

/// Response of `GET /daemon/disk-usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskUsage {
    /// Totals per resource type (images, containers, volumes).
    #[serde(default)]
    pub summaries: Vec<DiskUsageSummary>,
    /// Per-image details, as reported by the daemon.
    #[serde(default)]
    pub images: Vec<Value>,
    /// Per-container details, as reported by the daemon.
    #[serde(default)]
    pub containers: Vec<Value>,
    /// Per-volume details, as reported by the daemon.
    #[serde(default)]
    pub volumes: Vec<Value>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Disk usage totals for one resource type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskUsageSummary {
    /// Resource type, e.g. `Images`, `Containers`, `Local Volumes`.
    #[serde(rename = "type")]
    pub resource_type: String,
    /// Number of resources.
    #[serde(default)]
    pub total_count: u64,
    /// Number of resources in use.
    #[serde(default)]
    pub active: u64,
    /// Total size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Size that could be reclaimed, in bytes.
    #[serde(default)]
    pub reclaimable: u64,
}
