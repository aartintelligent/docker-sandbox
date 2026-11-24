//! A remote MCP server over streamable HTTP, reached from a sandbox with a
//! credential that the sandbox never sees, then rotated.
//!
//! ```sh
//! just example mcp-remote
//! ```
//!
//! The example serves the MCP tools over streamable HTTP on a loopback port of
//! the host, to requests carrying a bearer token only, and:
//!
//! 1. stores a dummy token as the `sbx` secret `mcp:<server>:token` — before
//!    the registration, since `sbx mcp add` probes the server;
//! 2. registers the server with `sbx mcp add --url … --header 'Authorization:
//!    Bearer ${token}'`, and loads it into the gateway of a shell sandbox;
//! 3. calls `echo` with the MCP client inside the sandbox: the gateway fills
//!    the header from the secret store of the host, so the server sees the
//!    token while the sandbox never holds it, which the example checks;
//! 4. rotates the token on the server, shows the gateway refused with the old
//!    secret, stores the new one, and shows what the gateway needs to use it.
//!
//! The secret and the registration are removed when the example ends.

use anyhow::{Context, Result, ensure};
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::sandbox::Sandbox;
use example_common::mcp::{self, HttpServer, Registration, SandboxClient};
use example_common::{connect, kit, sandbox};
use serde_json::json;

/// Placeholder of the token in the header of the server.
const PLACEHOLDER: &str = "token";

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("mcp-remote");
    // Dummy credentials, made for this run and removed with it.
    let token = mcp::dummy_token();
    let http = HttpServer::start(&token).await?;
    println!("MCP server listening on {}", http.url());

    let server = format!("{name}-remote");
    let header = format!("Authorization: Bearer ${{{PLACEHOLDER}}}");
    let (server, http, token) = (&server, &http, &token);
    mcp::set_secret(server, PLACEHOLDER, token).await?;
    let outcome: Result<()> = async {
        let registration =
            Registration::add_remote(&client, server, http.url(), &[&header]).await?;
        let outcome = sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
            let _running = sandbox.ensure_running().await?;
            let mcp_client = SandboxClient::install(&sandbox).await?;
            let url = sandbox
                .mcp_gateway()
                .await?
                .url
                .context("the gateway has no URL")?;

            sandbox.load_mcp_server(server).await?;
            let session = mcp_client.list(&url).await?;
            println!("the gateway lists {:?}", session.tools);
            println!("echo answered: {}", echo(&mcp_client, &url).await?);
            println!(
                "the server accepted {} requests carrying the token",
                http.authorized()
            );
            ensure!(http.authorized() > 0, "no request carried the token");
            ensure_absent(&sandbox, token).await?;

            let rotated = mcp::dummy_token();
            http.rotate_token(&rotated);
            let refused = http.refused();
            match echo(&mcp_client, &url).await {
                Ok(answer) => println!("token rotated on the server, echo still answers: {answer}"),
                Err(error) => {
                    println!("token rotated on the server, the old secret fails: {error:#}");
                }
            }
            println!("the server refused {} requests", http.refused() - refused);

            mcp::set_secret(server, PLACEHOLDER, &rotated).await?;
            match echo(&mcp_client, &url).await {
                Ok(answer) => println!("new secret stored, echo answers: {answer}"),
                Err(error) => {
                    println!("new secret stored, echo still fails: {error:#}");
                    sandbox.unload_mcp_server(server).await?;
                    sandbox.load_mcp_server(server).await?;
                    println!(
                        "server reloaded, echo answers: {}",
                        echo(&mcp_client, &url).await?
                    );
                }
            }
            ensure_absent(&sandbox, &rotated).await?;
            sandbox.unload_mcp_server(server).await?;
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

/// Calls `echo` through the gateway, from inside the sandbox.
async fn echo(client: &SandboxClient, url: &str) -> Result<String> {
    client
        .call(url, "echo", &json!({ "text": "hello from the sandbox" }))
        .await
}

/// Checks that `token` appears nowhere in the environment of the sandbox.
async fn ensure_absent(sandbox: &Sandbox, token: &str) -> Result<()> {
    let env = sandbox.exec(&ExecSpec::new(["env"])).await?.check()?;
    ensure!(
        !env.stdout.contains(token),
        "the token is visible inside the sandbox"
    );
    println!("the token is nowhere in the environment of the sandbox");
    Ok(())
}
