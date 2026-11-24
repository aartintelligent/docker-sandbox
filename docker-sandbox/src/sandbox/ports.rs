//! Ports published from a sandbox to the host.
//!
//! Publishing acts on the running sandbox and refuses a stopped one; the
//! daemon also keeps a *desired* configuration that it applies again at every
//! start.

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::Sandbox;
use crate::error::Result;
use crate::transport::Request;

/// Address the daemon binds published ports to by default.
const LOOPBACK: &str = "127.0.0.1";

/// A port published from a sandbox to the host.
///
/// Used both as request item (`POST /sandbox/{name}/ports`,
/// `…/ports/unpublish`) and as response item (`GET /sandbox/{name}/ports`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortBinding {
    /// Port inside the sandbox.
    pub sandbox_port: u16,
    /// Port on the host.
    pub host_port: u16,
    /// Address bound on the host. The daemon defaults to `127.0.0.1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_ip: Option<String>,
    /// Protocol, e.g. `tcp`, `tcp4` or `udp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
}

impl PortBinding {
    /// A TCP binding on the loopback address.
    ///
    /// ```
    /// use docker_sandbox::sandbox::PortBinding;
    ///
    /// let port = PortBinding::tcp(8080, 18080).with_host_ip("0.0.0.0");
    /// assert_eq!(port.host_ip.as_deref(), Some("0.0.0.0"));
    /// ```
    #[must_use]
    pub fn tcp(sandbox_port: u16, host_port: u16) -> Self {
        Self::with_protocol(sandbox_port, host_port, "tcp")
    }

    /// A UDP binding on the loopback address.
    #[must_use]
    pub fn udp(sandbox_port: u16, host_port: u16) -> Self {
        Self::with_protocol(sandbox_port, host_port, "udp")
    }

    /// A binding of `protocol` on the loopback address.
    fn with_protocol(sandbox_port: u16, host_port: u16, protocol: &str) -> Self {
        Self {
            sandbox_port,
            host_port,
            host_ip: Some(LOOPBACK.to_owned()),
            protocol: Some(protocol.to_owned()),
        }
    }

    /// Binds on `ip` instead of the loopback address.
    #[must_use]
    pub fn with_host_ip(mut self, ip: impl Into<String>) -> Self {
        self.host_ip = Some(ip.into());
        self
    }
}

impl Sandbox {
    /// Published ports (`GET /sandbox/{name}/ports`).
    ///
    /// # Errors
    ///
    /// Fails if the sandbox does not exist.
    pub async fn ports(&self) -> Result<Vec<PortBinding>> {
        let request = Request::get("sandbox ports", self.path("ports"));
        self.transport().json(request).await
    }

    /// Publishes ports (`POST /sandbox/{name}/ports`) and returns the
    /// resulting bindings.
    ///
    /// # Errors
    ///
    /// Fails with a conflict [`Error::Api`](crate::Error::Api) if the sandbox
    /// is stopped or a host port is already taken.
    pub async fn publish(&self, ports: &[PortBinding]) -> Result<Vec<PortBinding>> {
        let request = Request::post("port publication", self.path("ports")).json(ports)?;
        self.transport().json(request).await
    }

    /// Unpublishes ports (`POST /sandbox/{name}/ports/unpublish`). A port that
    /// is not published is ignored.
    ///
    /// # Errors
    ///
    /// Fails if the sandbox does not exist.
    pub async fn unpublish(&self, ports: &[PortBinding]) -> Result<()> {
        let request =
            Request::post("port unpublication", self.path("ports/unpublish")).json(ports)?;
        self.transport().unit(request).await
    }

    /// Replaces a port in the *desired* configuration, applied again when the
    /// sandbox starts (`POST /sandbox/{name}/ports/replace-desired`). Ports
    /// currently published are left untouched.
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api)
    /// (`desired_port_not_found`) if `old` is not part of the desired
    /// configuration.
    pub async fn replace_desired_port(&self, old: &PortBinding, new: &PortBinding) -> Result<()> {
        let request = Request::post(
            "desired port replacement",
            self.path("ports/replace-desired"),
        )
        .json(&json!({ "old": old, "new": new }))?;
        self.transport().unit(request).await
    }
}
