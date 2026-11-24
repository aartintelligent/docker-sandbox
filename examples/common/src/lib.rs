//! What the docker-sandbox examples share.
//!
//! Each example shows one use of the library; the plumbing they all need lives
//! here, so that their own code stays about the library:
//!
//! - [`connect`] reaches the daemon of the current user;
//! - [`kit::package`] compiles a kit once with the `sbx` CLI and caches it,
//!   [`kit::package_with_mixin`] a composition with a mixin, and
//!   [`kit::package_with_args`] a kit with arguments;
//! - [`sandbox::with_sandbox`] creates a sandbox and deletes it whatever
//!   happens;
//! - [`terminal::relay`] plugs the local terminal into an interactive
//!   execution, and [`host`] gives an agent session what `sbx run` passes
//!   from the host;
//! - [`credentials`] reads agent keys from the environment;
//! - [`program::install`] builds a program of this workspace for a sandbox
//!   and copies it there, [`program::install_tools`] the tools of
//!   `sandbox-tools`;
//! - [`mcp`] serves the MCP tools of the examples, registers them with
//!   `sbx`, and checks a gateway from inside a sandbox;
//! - [`agent`] runs a coding agent, with or without that server;
//! - [`automation`] runs one unattended on a task, and checks its work.

pub mod agent;
pub mod automation;
pub mod credentials;
pub mod host;
pub mod kit;
pub mod mcp;
pub mod program;
pub mod sandbox;
pub mod terminal;

use anyhow::{Context, Result};
use docker_sandbox::Client;

/// Connects to the daemon of the current user and reports its version.
///
/// # Errors
///
/// Fails when the daemon is not running or speaks an unsupported API version.
pub async fn connect() -> Result<Client> {
    let client = Client::connect()
        .await
        .context("cannot reach sandboxd: is Docker Sandboxes (sbx) installed and running?")?;
    let health = client.health();
    eprintln!("sandboxd {} (API {})", health.version, health.api_version);
    Ok(client)
}
