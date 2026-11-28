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
//! names; Claude Code keeps a reference to that variable, which it reads when
//! it connects. The example then sends, from inside the sandbox, the
//! `initialize` request Claude Code sends, to the URL and with the header it
//! registered, and checks that the server received the token. The mixin also
//! registers a remote server, whose bearer is read from a variable of the
//! sandbox, and a stdio server, a Python program Claude Code would start in
//! the sandbox: the example reads the registration of the first, never calls
//! it, and starts the second as Claude Code would. The servers are plain
//! data: the example prints them in the JSON form a program in another
//! language declares them in.
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
/// its first argument, and whether its bearer is a reference to the variable
/// named in the second.
const REGISTERED: &str = r#"
import json, sys
server = json.load(open("/home/agent/.claude.json"))["mcpServers"][sys.argv[1]]
bearer = server.get("headers", {}).get("Authorization") == "Bearer ${%s}" % sys.argv[2]
print(server["type"], server["url"], bearer)
"#;

/// The name Claude Code knows the stdio server of the example under.
const STDIO: &str = "example-files";

/// A stdio MCP server, run by `python3 -c`: it answers `initialize`, and
/// `-32601` to anything else.
const STDIO_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    if message.get("method") == "initialize":
        answer = {"result": {"protocolVersion": "2025-03-26", "capabilities": {},
                             "serverInfo": {"name": "example-files", "version": "0.0.0"}}}
    else:
        answer = {"error": {"code": -32601, "message": "method not found"}}
    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], **answer}), flush=True)
"#;

/// Starts the stdio server named in its argument with the command Claude
/// Code registered for it, as Claude Code would, sends it `initialize`, and
/// prints the type of the registration and the name the server answers with.
const START_STDIO: &str = r#"
import json, subprocess, sys
server = json.load(open("/home/agent/.claude.json"))["mcpServers"][sys.argv[1]]
request = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-03-26", "capabilities": {},
    "clientInfo": {"name": "agent-kit-forge", "version": "0.0.0"}}}
process = subprocess.run([server["command"], *server["args"]], input=json.dumps(request) + "\n",
                         capture_output=True, text=True, timeout=30)
print(server["type"], json.loads(process.stdout)["result"]["serverInfo"]["name"])
"#;

/// Sends, from inside the sandbox, the `initialize` request of an MCP client
/// to the URL and with the headers Claude Code registered for the server
/// named in its argument, their references to variables read as Claude Code
/// reads them, and prints the status of the answer.
const INITIALIZE: &str = r#"
import json, os, sys, urllib.request
server = json.load(open("/home/agent/.claude.json"))["mcpServers"][sys.argv[1]]
body = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-03-26", "capabilities": {},
    "clientInfo": {"name": "agent-kit-forge", "version": "0.0.0"}}}
headers = {k: os.path.expandvars(v) for k, v in server.get("headers", {}).items()}
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
        tools.url().unwrap_or_default()
    );
    // A remote server, called by the agent itself. A dummy bearer here; for a
    // real key, the placeholder of a `ProxiedKey` for its host, so that the
    // sandbox never holds the key.
    let docs = McpServer::remote("example-docs", REMOTE).with_bearer_env(REMOTE_TOKEN);
    // A server Claude Code starts in the sandbox, here a program given to
    // `python3 -c`.
    let files = McpServer::stdio(STDIO, "python3").with_args(["-c", STDIO_SERVER]);
    for server in [&tools, &docs] {
        println!("declared as JSON: {}", serde_json::to_string(server)?);
    }
    // The published kit of the claude agent, rebuilt from its image.
    let agent = Agent::Claude;
    eprintln!(
        "forging the package: {} rebuilt from its image, with model={MODEL} and reasoningEffort={EFFORT}",
        agent.reference()
    );
    let package = KitForge::agent(client.clone(), agent)
        .with_arg("model", MODEL)
        .with_arg("reasoningEffort", EFFORT)
        .with_mixin(Mixin::mcp(agent, [tools.clone(), docs.clone(), files])?)
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
        check_registrations(&sandbox, &docs).await?;
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

/// Checks the registrations of the remote server, never called, and of the
/// stdio server, started as Claude Code would start it.
async fn check_registrations(sandbox: &Sandbox, docs: &McpServer) -> Result<()> {
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
    let started = sandbox
        .exec(&ExecSpec::new(["python3", "-c", START_STDIO, STDIO]))
        .await?
        .check()?
        .stdout;
    ensure!(
        started.trim() == format!("stdio {STDIO}"),
        "unexpected stdio server: {}",
        started.trim()
    );
    println!("~/.claude.json registers {STDIO}, started as Claude Code would: it answers");
    Ok(())
}

/// The names of the argument declarations `args`, comma-separated.
fn names(args: &Value) -> String {
    args.as_object()
        .map(|declared| declared.keys().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}
