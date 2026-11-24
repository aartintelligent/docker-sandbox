//! An MCP server added from inside a sandbox, through the tools of its MCP
//! gateway, as an agent can do on its own.
//!
//! ```sh
//! just example mcp-dynamic
//! ```
//!
//! Besides the tools of its servers, the gateway of a sandbox offers tools of
//! its own: `mcp-find` searches the servers registered on the host, `mcp-add`
//! loads one into the gateway, `mcp-exec` calls a tool by name. The example
//! registers a remote MCP server on the host — the HTTP server of the
//! examples, with a dummy token stored as an `sbx` secret — without loading it
//! into the gateway. The MCP client inside the sandbox then finds it, adds it
//! and calls its tools.
//!
//! The addition outlives the session: the daemon lists the server among those
//! of the gateway, and the next session still offers its tools, until the
//! program unloads it through the socket. An agent can therefore load any
//! server registered on the host by itself; a sandbox created with a static
//! MCP set offers neither `mcp-find` nor `mcp-add` (see the `mcp-static`
//! example). The secret and the registration are removed when the example
//! ends.

use anyhow::{Context, Result, ensure};
use example_common::mcp::{self, HttpServer, Registration, SandboxClient};
use example_common::{connect, kit, sandbox};
use serde_json::{Value, json};

/// Placeholder of the token in the header of the server.
const PLACEHOLDER: &str = "token";

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("mcp-dynamic");
    let token = mcp::dummy_token();
    let http = HttpServer::start(&token).await?;
    let server = format!("{name}-remote");
    let header = format!("Authorization: Bearer ${{{PLACEHOLDER}}}");

    mcp::set_secret(&server, PLACEHOLDER, &token).await?;
    let (server, http) = (&server, &http);
    let outcome: Result<()> = async {
        let registration =
            Registration::add_remote(&client, server, http.url(), &[&header]).await?;
        let outcome = sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
            let _running = sandbox.ensure_running().await?;
            let mcp_client = SandboxClient::install(&sandbox).await?;
            let gateway = sandbox.mcp_gateway().await?;
            println!("servers loaded by the daemon: {:?}", gateway.servers);
            let url = gateway.url.context("the gateway has no URL")?;

            let session = mcp_client
                .session(
                    &url,
                    &[
                        ("mcp-find", json!({ "query": server })),
                        ("mcp-add", json!({ "name": server })),
                        ("sum", json!({ "a": 2, "b": 3 })),
                        (
                            "mcp-exec",
                            json!({ "name": "echo", "arguments": { "text": "added from inside" } }),
                        ),
                    ],
                )
                .await?;
            println!("the session opens with {:?}", session.tools);
            let mut calls = session.calls.into_iter();
            let found = calls.next().context("no answer to mcp-find")?.into_text()?;
            println!("mcp-find {server}: {}", names(&found));
            let added = calls.next().context("no answer to mcp-add")?.into_text()?;
            println!("mcp-add {server}: {}", added.trim());
            let sum = calls.next().context("no answer to sum")?.into_text()?;
            println!("sum(2, 3) = {sum}");
            let echo = calls.next().context("no answer to mcp-exec")?.into_text()?;
            println!("mcp-exec echo: {echo}");
            println!("the session then offers {:?}", session.tools_after);
            ensure!(sum == "5", "the added server answered {sum}, not 5");

            println!(
                "the gateway reached the server with the token {} times",
                http.authorized()
            );
            let gateway = sandbox.mcp_gateway().await?;
            println!("servers loaded by the daemon: {:?}", gateway.servers);
            ensure!(
                gateway.servers.contains(server),
                "the daemon does not list the server added from inside"
            );
            let next = mcp_client.list(&url).await?;
            println!("a new session offers {:?}", next.tools);

            sandbox.unload_mcp_server(server).await?;
            let next = mcp_client.list(&url).await?;
            println!(
                "unloaded through the socket: a new session offers {:?}",
                next.tools
            );
            ensure!(next.tool("sum").is_none(), "the server is still offered");
            Ok(())
        })
        .await;
        registration.remove().await?;
        outcome
    }
    .await;
    mcp::remove_secret(server, PLACEHOLDER).await?;
    outcome
}

/// The names in the answer of `mcp-find`, a JSON list of `{name, type}`, or
/// the answer itself when it is not one.
fn names(found: &str) -> String {
    serde_json::from_str::<Vec<Value>>(found).map_or_else(
        |_| found.trim().to_owned(),
        |servers| {
            let names: Vec<&str> = servers.iter().filter_map(|s| s["name"].as_str()).collect();
            format!("{names:?}")
        },
    )
}
