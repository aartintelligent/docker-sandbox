//! The arguments of the `claude` kit chosen without the `sbx` CLI, and an MCP
//! server given to Claude Code by a mixin: the same sandbox as
//! [`agent-kit-args`](../../agent-kit-args/src/main.rs), from a package forged
//! through the socket alone.
//!
//! `KitForge` rebuilds the kit from its published image: the daemon pulls and
//! exports it, and the descriptor of the kit is read from the image. The
//! values given with `KitForge::with_arg` are checked as the CLI checks them,
//! against the values or the pattern the kit declares: a value the kit
//! refuses, or an argument it does not declare, is an error before anything
//! is created. They are then substituted in the commands of the kit and set
//! in its variables: the sandbox gets `CLAUDE_MODEL` and
//! `CLAUDE_REASONING_EFFORT`, and the install command of the kit writes them
//! into `~/.claude/settings.json`.
//!
//! The MCP server of the examples is served by this program on a loopback
//! port of the host. `Mixin::mcp` registers it in Claude Code at install time
//! (`claude mcp add`, inside the sandbox): no file is written on the host and
//! nothing is registered with the daemon. Its bearer token, random and
//! different at every run, is given at creation in the variable the server
//! names. The example then sends, from inside the sandbox, the `initialize`
//! request Claude Code sends, to the URL and with the header it registered,
//! and checks that the server received the token. The mixin also registers a
//! remote server, whose bearer is read from a variable of the sandbox: the
//! example only reads its registration, and never calls it.
//!
//! The kit is resolved as the CLI resolves it, the order of its effective
//! capabilities included: the daemon authorizes the credential it declares,
//! `anthropic`, as for a sandbox the CLI creates, and the proxy would write
//! the credential stored for it into the requests of the agent. The example
//! never starts Claude, so that none is sent.
//!
//! ```sh
//! just example agent-kit-forge
//! ```

use anyhow::{Context, Result, ensure};
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::sandbox::Sandbox;
use docker_sandbox_kit::{Agent, KitForge, McpServer, Mixin};
use example_common::mcp::{HttpServer, dummy_token};
use example_common::{connect, sandbox};
use serde_json::Value;

/// Where the package is written.
const OUTPUT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/forged/agent-kit-forge"
);

/// The model chosen for the sandbox.
const MODEL: &str = "claude-haiku-5-5";

/// The effort chosen for the sandbox.
const EFFORT: &str = "low";

/// The name Claude Code knows the MCP server of the examples under.
const SERVER: &str = "example-tools";

/// The remote MCP server registered beside it, in a reserved domain: never
/// called.
const REMOTE: &str = "https://mcp.example.com/mcp";

/// The variable of the sandbox holding the bearer of the remote server.
const REMOTE_TOKEN: &str = "EXAMPLE_DOCS_TOKEN";

/// Prints the type and URL Claude Code registered for the server named in
/// its first argument, and whether its bearer is the content of the variable
/// named in the second.
const REGISTERED: &str = r#"
import json, os, sys
server = json.load(open("/home/agent/.claude.json"))["mcpServers"][sys.argv[1]]
bearer = server.get("headers", {}).get("Authorization") == "Bearer " + os.environ[sys.argv[2]]
print(server["type"], server["url"], bearer)
"#;

/// Sends, from inside the sandbox, the `initialize` request of an MCP client
/// to the URL and with the headers Claude Code registered for the server
/// named in its argument, and prints the status of the answer.
const INITIALIZE: &str = r#"
import json, sys, urllib.request
server = json.load(open("/home/agent/.claude.json"))["mcpServers"][sys.argv[1]]
body = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-03-26", "capabilities": {},
    "clientInfo": {"name": "agent-kit-forge", "version": "0.0.0"}}}
headers = dict(server.get("headers", {}))
headers.update({"Content-Type": "application/json", "Accept": "application/json, text/event-stream"})
request = urllib.request.Request(server["url"], json.dumps(body).encode(), headers)
print(server["type"], server["url"], urllib.request.urlopen(request, timeout=30).status)
"#;

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let known: Vec<&str> = Agent::ALL.iter().map(|agent| agent.name()).collect();
    println!("the agents sbx knows: {}", known.join(", "));
    let token = dummy_token();
    let http = HttpServer::start(&token).await?;
    // The server the sandbox reaches on the loopback of the host.
    let tools = McpServer::host(SERVER, http.port());
    println!(
        "MCP server listening on {}, given to Claude Code as {} at {}",
        http.url(),
        tools.name(),
        tools.url()
    );
    // A remote server, called by the agent itself. A dummy bearer here; for a
    // real key, the placeholder of a `ProxiedKey` for its host, so that the
    // sandbox never holds the key.
    let docs = McpServer::remote("example-docs", REMOTE).with_bearer_env(REMOTE_TOKEN);
    // The published kit of the claude agent, rebuilt from its image.
    let agent = Agent::Claude;
    eprintln!(
        "forging the package: {} rebuilt from its image, with model={MODEL} and reasoningEffort={EFFORT}",
        agent.reference()
    );
    let package = KitForge::agent(client.clone(), agent)
        .with_arg("model", MODEL)
        .with_arg("reasoningEffort", EFFORT)
        .with_mixin(Mixin::mcp(agent, [tools.clone(), docs.clone()])?)
        .forge(OUTPUT)
        .await?;
    let artifact = package
        .kit_artifacts()
        .into_iter()
        .next()
        .context("the package has no kit")?;
    println!(
        "the kit declares the arguments {}; their values: {}",
        names(&artifact["args"]),
        artifact["v3ArgValues"]
    );

    let name = sandbox::unique_name("kit-forge");
    let token_env = tools
        .token_env()
        .context("a server of the host takes a token")?;
    let request = package
        .request(&name)?
        .with_env(token_env, &token)
        .with_env(REMOTE_TOKEN, dummy_token());
    sandbox::with_sandbox_from(&client, &package, &request, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        let env = sandbox
            .exec(&ExecSpec::new([
                "printenv",
                "CLAUDE_MODEL",
                "CLAUDE_REASONING_EFFORT",
            ]))
            .await?
            .check()?
            .stdout;
        let env: Vec<&str> = env.lines().collect();
        ensure!(env == [MODEL, EFFORT], "unexpected environment: {env:?}");
        println!(
            "environment of the sandbox: CLAUDE_MODEL={MODEL}, CLAUDE_REASONING_EFFORT={EFFORT}"
        );

        // Only the two fields the install command writes from the arguments
        // are read from the settings.
        let settings = sandbox
            .read_file("/home/agent/.claude/settings.json")
            .await?;
        let settings: Value = serde_json::from_slice(&settings).context("settings.json is JSON")?;
        ensure!(
            settings["model"] == MODEL && settings["effortLevel"] == EFFORT,
            "unexpected settings: model {}, effortLevel {}",
            settings["model"],
            settings["effortLevel"]
        );
        println!("~/.claude/settings.json: model {MODEL}, effortLevel {EFFORT}");
        check_server(&sandbox, &http).await?;
        let registered = sandbox
            .exec(&ExecSpec::new([
                "python3",
                "-c",
                REGISTERED,
                docs.name(),
                REMOTE_TOKEN,
            ]))
            .await?
            .check()?
            .stdout;
        ensure!(
            registered.trim() == format!("http {REMOTE} True"),
            "unexpected registration of {}: {}",
            docs.name(),
            registered.trim()
        );
        println!(
            "~/.claude.json registers {} at {REMOTE}, its bearer read from {REMOTE_TOKEN}",
            docs.name()
        );
        Ok(())
    })
    .await
}

/// Checks that Claude Code got the MCP server of the mixin, and that the
/// server is reached from the sandbox with the token given at creation,
/// without starting Claude.
async fn check_server(sandbox: &Sandbox, http: &HttpServer) -> Result<()> {
    let answered = sandbox
        .exec(&ExecSpec::new(["python3", "-c", INITIALIZE, SERVER]))
        .await?
        .check()?
        .stdout;
    println!(
        "~/.claude.json registers {SERVER}, which answers: {}",
        answered.trim()
    );
    ensure!(
        answered.trim().ends_with(" 200"),
        "the server did not answer the registered request"
    );
    ensure!(
        http.authorized() > 0 && http.refused() == 0,
        "the server received {} requests with its token, {} without",
        http.authorized(),
        http.refused()
    );
    println!(
        "the server received the token given at creation ({} request)",
        http.authorized()
    );
    Ok(())
}

/// The names of the argument declarations `args`, comma-separated.
fn names(args: &Value) -> String {
    args.as_object()
        .map(|declared| declared.keys().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}
