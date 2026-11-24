//! An MCP server loaded into the gateway of a sandbox, then checked from
//! inside the sandbox with an MCP client, the way the agent of the sandbox
//! would use it.
//!
//! ```sh
//! just example mcp-gateway
//! ```
//!
//! The example registers its own binary as a local MCP server (`sbx mcp add`),
//! loads it into the gateway of a shell sandbox, and runs the `mcp-client`
//! example inside the sandbox: through `http://mcp-gateway.docker.internal/mcp`
//! it lists the tools and calls `sum`. It then shows that the port of the
//! gateway on the host refuses a request that does not come through the proxy
//! of the sandbox, and what the client lists once the server is unloaded. The
//! registration is removed when the example ends.

use anyhow::{Context, Result, ensure};
use example_common::mcp::{self, Registration, SandboxClient};
use example_common::{connect, kit, sandbox};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() -> Result<()> {
    // `sbx` starts this same binary as the MCP server.
    if mcp::serve_if_requested().await? {
        return Ok(());
    }
    let client = connect().await?;
    let mode = client.mcp().gateway_mode().await?;
    println!("MCP gateway of the daemon: {}", mode.decision);
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("mcp-gateway");
    let registration = Registration::add(&client, &format!("{name}-tools")).await?;
    let server = registration.name().to_owned();

    let outcome = sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        let mcp_client = SandboxClient::install(&sandbox).await?;

        sandbox.load_mcp_server(&server).await?;
        let gateway = sandbox.mcp_gateway().await?;
        println!(
            "gateway {}: servers {:?}, static {}",
            gateway.state.as_deref().unwrap_or("?"),
            gateway.servers,
            gateway.static_servers
        );
        let url = gateway.url.context("the gateway has no URL")?;

        let listing = mcp_client.list(&url).await?;
        println!("{url} ({}) lists {:?}", listing.server, listing.tools);
        let sum = listing
            .tool("sum")
            .context("the gateway lists no sum tool")?;
        let answer = mcp_client
            .call(&url, sum, &json!({ "a": 2, "b": 3 }))
            .await?;
        println!("{sum}(2, 3) = {answer}");
        ensure!(answer == "5", "the server answered {answer}, not 5");

        if let Some(port) = gateway.port {
            println!(
                "from the host, port {port} of the gateway answers: {}",
                status_line(port).await?
            );
        }

        sandbox.unload_mcp_server(&server).await?;
        let listing = mcp_client.list(&url).await?;
        println!(
            "once {server} is unloaded, the gateway lists {:?}",
            listing.tools
        );
        Ok(())
    })
    .await;

    registration.remove().await?;
    outcome
}

/// The status line of the answer to an MCP request sent straight to `port`
/// on the host, without the proxy of a sandbox.
async fn status_line(port: u16) -> Result<String> {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream.write_all(request.as_bytes()).await?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await?;
    Ok(answer.lines().next().unwrap_or_default().to_owned())
}
