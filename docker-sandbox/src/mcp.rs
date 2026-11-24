//! MCP: the gateway of each sandbox and the servers loaded into it.
//!
//! MCP servers are registered on the host with `sbx mcp add`; the daemon then
//! loads them into the gateway of a sandbox on demand. Daemon-wide operations,
//! such as checking a server against the policy before registering it, are
//! reached with [`Client::mcp`](crate::Client::mcp), sandbox operations from
//! the [`Sandbox`] handle.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Extra;
use crate::client::Client;
use crate::error::Result;
use crate::sandbox::Sandbox;
use crate::transport::{Request, segment};

/// MCP gateway of a sandbox (`GET /sandbox/{name}/mcp/gateway`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpGateway {
    /// Gateway state, e.g. `ready`.
    #[serde(default)]
    pub state: Option<String>,
    /// Gateway URL as seen from inside the sandbox.
    #[serde(default)]
    pub url: Option<String>,
    /// Port of the gateway on the host.
    #[serde(default)]
    pub port: Option<u16>,
    /// Whether the set of MCP servers was fixed at creation time. The daemon
    /// still loads and unloads servers on request in that case.
    #[serde(rename = "static", default)]
    pub static_servers: bool,
    /// Servers loaded into the gateway.
    #[serde(default)]
    pub servers: Vec<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Gateway mode of the daemon (`GET /mcp/gateway-mode`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayMode {
    /// Gateway in use, e.g. `local`.
    pub decision: String,
    /// URL of a remote gateway, `none` for the local one.
    #[serde(default)]
    pub gateway_url: Option<String>,
    /// Why this gateway is used.
    #[serde(default)]
    pub reason: Option<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A server loaded into the gateway of a sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadedServer {
    /// Sandbox name.
    pub sandbox: String,
    /// Server name.
    pub server: String,
}

/// A server to check against the MCP policy, sent to
/// `POST /mcp/registration/check`.
///
/// The daemon accepts these fields alone; the `sbx` CLI sends a larger
/// structure with every option of a registration.
///
/// ```
/// use docker_sandbox::mcp::McpServerCheck;
///
/// let local = McpServerCheck::local("docs", ["npx", "-y", "@acme/docs-mcp"]);
/// let remote = McpServerCheck::remote("search", "https://mcp.example.com/mcp");
/// # let _ = (local, remote);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerCheck {
    /// Server name.
    #[serde(rename = "Name")]
    pub name: String,
    /// Where the server runs.
    #[serde(rename = "Type")]
    pub kind: McpServerKind,
    /// Program and arguments of a local server.
    #[serde(rename = "Command", default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// URL of a remote server.
    #[serde(rename = "URL", default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl McpServerCheck {
    /// A server started on the host with `command`.
    pub fn local(
        name: impl Into<String>,
        command: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            kind: McpServerKind::Local,
            command: command.into_iter().map(Into::into).collect(),
            url: None,
        }
    }

    /// A server reached at `url`.
    pub fn remote(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: McpServerKind::Remote,
            command: Vec::new(),
            url: Some(url.into()),
        }
    }
}

/// Where an MCP server runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum McpServerKind {
    /// Started on the host.
    Local,
    /// Reached over the network.
    Remote,
}

/// Decision of the policy for an [`McpServerCheck`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistrationCheck {
    /// Whether the server may be registered.
    pub allowed: bool,
    /// Server name.
    pub name: String,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Daemon-wide MCP operations. Obtained with [`Client::mcp`].
#[derive(Debug, Clone)]
pub struct Mcp {
    /// Client the requests go through.
    client: Client,
}

impl Mcp {
    /// MCP operations through `client`.
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Gateway mode of the daemon (`GET /mcp/gateway-mode`).
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn gateway_mode(&self) -> Result<GatewayMode> {
        let request = Request::get("MCP gateway mode", "/mcp/gateway-mode");
        self.client.transport().json(request).await
    }

    /// Checks whether the policy allows registering `server`
    /// (`POST /mcp/registration/check`). Nothing is registered.
    ///
    /// # Errors
    ///
    /// Fails with an invalid-input [`Error::Api`](crate::Error::Api) if the
    /// description is rejected.
    pub async fn check_registration(&self, server: &McpServerCheck) -> Result<RegistrationCheck> {
        let request = Request::post("MCP registration check", "/mcp/registration/check")
            .json(&json!({ "spec": server }))?;
        self.client.transport().json(request).await
    }
}

impl Sandbox {
    /// MCP gateway of the sandbox (`GET /sandbox/{name}/mcp/gateway`).
    ///
    /// # Errors
    ///
    /// Fails if the sandbox does not exist.
    pub async fn mcp_gateway(&self) -> Result<McpGateway> {
        let request = Request::get("MCP gateway", self.path("mcp/gateway"));
        self.transport().json(request).await
    }

    /// Loads the registered MCP server `server` into the gateway of the
    /// sandbox (`POST /sandbox/{name}/mcp/gateway/servers`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) if the server
    /// is not registered with `sbx mcp add`.
    pub async fn load_mcp_server(&self, server: &str) -> Result<LoadedServer> {
        let request = Request::post("MCP server load", self.path("mcp/gateway/servers"))
            .json(&json!({ "server": server }))?;
        self.transport().json(request).await
    }

    /// Unloads the MCP server `server` from the gateway of the sandbox
    /// (`DELETE /sandbox/{name}/mcp/gateway/servers/{server}`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) if the server
    /// is not loaded in the sandbox.
    pub async fn unload_mcp_server(&self, server: &str) -> Result<()> {
        let path = format!("{}/{}", self.path("mcp/gateway/servers"), segment(server));
        self.transport()
            .unit(Request::delete("MCP server unload", path))
            .await
    }
}
