//! A minimal MCP client over streamable HTTP, built with `rmcp`: it connects
//! to a server, lists its tools, and calls some of them in one session.
//!
//! ```sh
//! example-mcp-client <url> [<tool> <json arguments>]...
//! example-mcp-client http://mcp-gateway.docker.internal/mcp sum '{"a": 2, "b": 3}'
//! ```
//!
//! It prints one JSON object per line — `{"server": …}`, `{"tools": [...]}`,
//! one `{"tool": …, "result": …}` per call, then `{"tools": [...]}` again
//! when a call was made, since a call may change the tools of the session —
//! so that a program can read what it saw. The `mcp-*` examples build it as a
//! static binary and run it inside the sandbox, against the MCP gateway the
//! agents of the sandbox use: the HTTP proxy of the sandbox adds the
//! credential of the gateway on the way.

use anyhow::{Context, Result, bail};
use rmcp::model::{CallToolRequestParams, JsonObject};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::json;

/// How the program is called.
const USAGE: &str = "usage: example-mcp-client <url> [<tool> <json arguments>]...";

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args.next().context(USAGE)?;
    let calls: Vec<String> = args.collect();
    if !calls.len().is_multiple_of(2) {
        bail!(USAGE);
    }
    let client = ()
        .serve(StreamableHttpClientTransport::from_uri(url.clone()))
        .await
        .with_context(|| format!("connecting to the MCP server at {url}"))?;

    let server = client
        .peer_info()
        .and_then(|info| info.server_info.clone())
        .map(|implementation| implementation.name)
        .unwrap_or_default();
    println!("{}", json!({ "server": server }));
    print_tools(&client).await?;

    for call in calls.chunks_exact(2) {
        let arguments: JsonObject =
            serde_json::from_str(&call[1]).context("parsing the tool arguments")?;
        let params = CallToolRequestParams::new(call[0].clone()).with_arguments(arguments);
        let result = client
            .call_tool(params)
            .await
            .with_context(|| format!("calling the tool {}", call[0]))?;
        println!("{}", json!({ "tool": call[0], "result": result }));
    }
    if !calls.is_empty() {
        print_tools(&client).await?;
    }
    client.cancel().await.context("closing the session")?;
    Ok(())
}

/// Prints the names of the tools the session offers.
async fn print_tools(client: &RunningService<RoleClient, ()>) -> Result<()> {
    let tools = client.list_all_tools().await.context("listing the tools")?;
    let names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
    println!("{}", json!({ "tools": names }));
    Ok(())
}
