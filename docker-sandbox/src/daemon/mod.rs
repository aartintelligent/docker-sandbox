//! Daemon-wide operations: health, information, settings, log levels, disk
//! usage and diagnostics.
//!
//! Obtained with [`Client::daemon`](crate::Client::daemon). The destructive
//! routes of the daemon (`/daemon/reset`, `/daemon/shutdown`) are deliberately
//! not exposed.

mod types;

use std::collections::BTreeMap;

use serde_json::{Value, json};

use self::types::SettingsResponse;
pub use self::types::{DaemonInfo, DiskUsage, DiskUsageSummary, Health, Setting};
use crate::client::Client;
use crate::error::Result;
use crate::transport::{Request, segment};

/// Daemon-wide operations. Obtained with [`Client::daemon`].
#[derive(Debug, Clone)]
pub struct Daemon {
    /// Client the requests go through.
    client: Client,
}

impl Daemon {
    /// Daemon operations through `client`.
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Current health and version (`GET /daemon/health`).
    ///
    /// [`Client::health`] gives the health reported at connection time
    /// without a request.
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn health(&self) -> Result<Health> {
        let request = Request::get("daemon health", "/daemon/health");
        self.client.transport().json(request).await
    }

    /// Daemon information (`GET /daemon/info`).
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn info(&self) -> Result<DaemonInfo> {
        let request = Request::get("daemon info", "/daemon/info");
        self.client.transport().json(request).await
    }

    /// Every setting with its effective value (`GET /daemon/settings?all=true`).
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn settings(&self) -> Result<Vec<Setting>> {
        let request = Request::get("daemon settings", "/daemon/settings").query("all", "true");
        let response: SettingsResponse = self.client.transport().json(request).await?;
        Ok(response.settings)
    }

    /// One setting (`GET /daemon/settings/{key}`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) for an unknown
    /// key.
    pub async fn setting(&self, key: &str) -> Result<Setting> {
        let request = Request::get("daemon setting", setting_path(key));
        self.client.transport().json(request).await
    }

    /// Overrides a setting (`POST /daemon/settings/{key}`) and returns its new
    /// state.
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) for an
    /// unknown key, and with the daemon error if the value is rejected.
    pub async fn set_setting(&self, key: &str, value: impl Into<Value>) -> Result<Setting> {
        let request = Request::post("daemon setting update", setting_path(key))
            .json(&json!({ "value": value.into() }))?;
        self.client.transport().json(request).await
    }

    /// Removes the override of a setting, restoring its default
    /// (`DELETE /daemon/settings/{key}`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) for an
    /// unknown key.
    pub async fn unset_setting(&self, key: &str) -> Result<()> {
        let request = Request::delete("daemon setting reset", setting_path(key));
        self.client.transport().unit(request).await
    }

    /// Log level of each category (`GET /daemon/loglevel`), e.g.
    /// `{"general": "info", "proxy": "info"}`.
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn log_levels(&self) -> Result<BTreeMap<String, String>> {
        let request = Request::get("daemon log levels", "/daemon/loglevel");
        self.client.transport().json(request).await
    }

    /// Sets the log level of a category (`POST /daemon/loglevel/{target}`).
    /// `target` is `general`, `proxy` or `all`; `level` is `debug`, `info`,
    /// `warn` or `error`.
    ///
    /// # Errors
    ///
    /// Fails with an invalid-input [`Error::Api`](crate::Error::Api) for an
    /// unknown category or level.
    pub async fn set_log_level(&self, target: &str, level: &str) -> Result<()> {
        let path = format!("/daemon/loglevel/{}", segment(target));
        let request =
            Request::post("daemon log level update", path).json(&json!({ "level": level }))?;
        self.client.transport().unit(request).await
    }

    /// Disk usage of images, containers and volumes (`GET /daemon/disk-usage`).
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn disk_usage(&self) -> Result<DiskUsage> {
        let request = Request::get("daemon disk usage", "/daemon/disk-usage");
        self.client.transport().json(request).await
    }

    /// Internal state dump (`GET /daemon/debug/state`). The shape is not
    /// stable; secret values are masked by the daemon.
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn debug_state(&self) -> Result<Value> {
        let request = Request::get("daemon debug state", "/daemon/debug/state");
        self.client.transport().json(request).await
    }

    /// Diagnostics report (`GET /daemon/diagnostics`). The shape is not stable.
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn diagnostics(&self) -> Result<Value> {
        let request = Request::get("daemon diagnostics", "/daemon/diagnostics").long_running();
        self.client.transport().json(request).await
    }
}

/// `/daemon/settings/{key}`.
fn setting_path(key: &str) -> String {
    format!("/daemon/settings/{}", segment(key))
}
