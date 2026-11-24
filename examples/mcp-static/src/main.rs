//! A sandbox created with a fixed set of MCP servers, as
//! `sbx create --static-mcp` does.
//!
//! ```sh
//! just example mcp-static
//! ```
//!
//! The example registers its own binary as a local MCP server, then creates a
//! shell sandbox whose creation request names it as the static MCP set
//! (`CreateRequest::with_static_mcp_servers`): the gateway serves it from the
//! start, without a load. The MCP client inside the sandbox lists the tools
//! and calls `host_time`.
//!
//! A static gateway offers neither `mcp-find` nor `mcp-add`: the sandbox
//! cannot load other servers by itself, which a dynamic gateway lets it do
//! (see the `mcp-dynamic` example). The program keeps that power: the daemon
//! still unloads and loads servers of a static set through the socket, and the
//! example shows the listing after each. The registration is removed when the
//! example ends.

use anyhow::{Context, Result, ensure};
use example_common::mcp::{self, Registration, SandboxClient};
use example_common::{connect, kit, sandbox};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    // `sbx` starts this same binary as the MCP server.
    if mcp::serve_if_requested().await? {
        return Ok(());
    }
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("mcp-static");
    let registration = Registration::add(&client, &format!("{name}-tools")).await?;
    let server = registration.name().to_owned();
    let outcome = async {
        let request = package
            .request(&name)?
            .with_static_mcp_servers([server.as_str()]);
        sandbox::with_sandbox_from(&client, &package, &request, |sandbox| async move {
            let _running = sandbox.ensure_running().await?;
            let mcp_client = SandboxClient::install(&sandbox).await?;
            let gateway = sandbox.mcp_gateway().await?;
            println!(
                "gateway {}: servers {:?}, static {}",
                gateway.state.as_deref().unwrap_or("?"),
                gateway.servers,
                gateway.static_servers
            );
            let url = gateway.url.context("the gateway has no URL")?;

            let listing = mcp_client.list(&url).await?;
            println!("without any load, the gateway lists {:?}", listing.tools);
            ensure!(
                listing.tool("mcp-add").is_none() && listing.tool("mcp-find").is_none(),
                "a static gateway offers mcp-add or mcp-find"
            );
            println!("no mcp-find nor mcp-add: the sandbox cannot load servers by itself");
            let host_time = listing
                .tool("host_time")
                .context("the gateway lists no host_time tool")?;
            let answer = mcp_client.call(&url, host_time, &json!({})).await?;
            println!("{host_time}() = {answer}");

            sandbox.unload_mcp_server(&server).await?;
            let listing = mcp_client.list(&url).await?;
            println!("unloaded from the static set: {:?}", listing.tools);
            sandbox.load_mcp_server(&server).await?;
            let listing = mcp_client.list(&url).await?;
            println!("loaded again: {:?}", listing.tools);
            Ok(())
        })
        .await
    }
    .await;

    registration.remove().await?;
    outcome
}
