//! An agentic task in a sandbox, described in a few lines of Rust: Claude
//! Code fixes a project whose tests fail, with a tool of this program at
//! hand, and the program checks its work.
//!
//! # Why a spec
//!
//! Running an agent on a task in a sandbox takes a lot of plumbing when it is
//! done by hand, as the `agent-*-automation` examples do: rebuild the kit of
//! the agent from its image, compose the mixins, create the sandbox with the
//! right variables, keep a session open so that it runs, give the API key to
//! the proxy so that the sandbox never holds it, register the MCP servers in
//! the agent with a token of their own, scope the network, and remove it all
//! when the work is done or fails. A `SandboxSpec` says what the sandbox
//! needs, and `SandboxSpec::launch` does all of that in one call:
//!
//! - the kit is forged from its published image, without the `sbx` CLI;
//! - the API key reaches the agent through the proxy alone: the sandbox only
//!   holds a placeholder, and the key is never written in it;
//! - an MCP server of the host gets a random token, drawn for this sandbox
//!   and this run, which the server checks;
//! - the network rules come with the sandbox and go with it;
//! - everything is checked before anything is created, and a launch that
//!   fails half-way removes what it created;
//! - the `Lease` it returns owns the sandbox: dropping the work is a call to
//!   `shutdown`, which leaves nothing behind.
//!
//! The same spec serves every sandbox of a job; it holds nothing secret, the
//! key being given at launch.
//!
//! # What the example does
//!
//! 1. It serves the MCP tools of the examples on a loopback port of the host,
//!    with `rmcp`, for the agent to call.
//! 2. It writes the project on the host: a small calculator whose `sub` is
//!    wrong, and a pristine copy to compare with afterwards.
//! 3. It describes the sandbox: the `claude` kit, the project as its
//!    workspace, the copy read-only, the tools, a network refusal, and the key
//!    of the agent when the environment holds one.
//! 4. It launches the sandbox, and runs the task: the agent the kit declares,
//!    in its non-interactive mode, with a prompt.
//! 5. It checks the work without trusting the agent: the tests of the project
//!    before and after, the diff against the copy, and the requests the tools
//!    received with the token of the sandbox. Then it shuts the lease down.
//!
//! The project stays on the host, under `examples/target/automation/`.
//!
//! # Running it
//!
//! ```sh
//! just example blueprint
//! ```
//!
//! With `ANTHROPIC_API_KEY` in the environment, the agent uses that key,
//! through the proxy. Without it, the agent relies on the credential the
//! daemon holds for its kit, a subscription for instance.

use anyhow::{Context, Result, anyhow, ensure};
// One import for the whole example: the prelude holds the types of this
// crate and those of the two other crates a spec is written with. The
// `Result` of `anyhow`, imported by name, wins over the one of the glob.
use docker_sandbox_blueprint::prelude::*;
use example_common::automation::{self, AGENT_DEADLINE};
use example_common::mcp::{HttpServer, dummy_token};
use example_common::{connect, sandbox};

/// The name the agent knows the tools of this program under.
const TOOLS: &str = "example-tools";

/// The variable the agent reads its API key from.
const KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// The service of the claude kit the key belongs to: the kit declares the
/// hosts of its API, so that the spec does not have to write them out.
const KEY_SERVICE: &str = "anthropic";

/// A host the sandbox may not reach, whatever else allows it.
const DENIED: &str = "*.pastebin.com";

/// The task given to the agent: fix the project, then use a tool of this
/// program, so that the example shows the agent reaching it.
const PROMPT: &str = "The test suite of this project fails: run `sh test.sh` to see it. Fix the \
     bug in calc.sh so that every test passes, without changing test.sh. Then call the `sum` \
     tool of the example-tools MCP server with a = 2 and b = 3, and check that \
     `sh calc.sh add 2 3` prints the same number. Summarize the fix in one sentence.";

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;

    // Step 1: the tools of the program, served on the host. They refuse
    // every request until they know the token of the sandbox, drawn at
    // launch.
    let tools = HttpServer::start(&dummy_token()).await?;
    eprintln!("step 1: the MCP tools listen on {}", tools.url());

    // Step 2: the project the agent works on, on the host.
    let name = sandbox::unique_name("blueprint");
    let (project, original) = automation::write_project(&name)?;
    eprintln!("step 2: the project is written to {project}");

    // Step 3: the sandbox, described.
    let (spec, keys) = describe(&project, &original, tools.port());
    eprintln!("step 3: the sandbox is described");

    // Step 4: the launch. One call forges the kit, checks the spec against
    // the daemon, draws the token of the tools, gives the key to the proxy,
    // creates and starts the sandbox. A failure on the way removes what was
    // created.
    eprintln!("step 4: launching sandbox {name}");
    let lease = spec.launch(&client, &name, &keys).await?;

    // The task and its check run until they end, or until Ctrl-C; the lease
    // is shut down in every case, so that no sandbox is left behind.
    let outcome = tokio::select! {
        outcome = work(&lease, &tools, &project, &original) => outcome,
        _ = tokio::signal::ctrl_c() => Err(anyhow!("interrupted")),
    };
    eprintln!("shutting down sandbox {name}: it goes, with its rules and its key");
    let shut = lease.shutdown().await;
    outcome?;
    shut?;
    Ok(())
}

/// Step 3: the sandbox of the task, and the values of its keys.
///
/// Each line of the spec is a need of the task; the comments say what the
/// launch does with it.
fn describe(project: &str, original: &str, tools_port: u16) -> (SandboxSpec, Keys) {
    let mut spec = SandboxSpec::agent(Agent::Claude)
        // The project, mounted read-write at its own path: the agent changes
        // the files of the host, and nothing has to be copied back.
        .with_workspace(project)
        // The pristine copy, read-only: the program compares with it, and
        // the agent cannot alter it.
        .with_mount(Workspace::new(original).read_only())
        // The tools of this program. The launch registers them in Claude
        // Code, at `host.docker.internal`, with a token drawn for this
        // sandbox, which `Lease::mcp_token` gives to the program.
        .with_mcp(McpServer::host(TOOLS, tools_port))
        // A host refused from the creation of the sandbox, its install
        // commands included; the rule goes with the sandbox.
        .with_network_deny([DENIED]);

    // The key of the agent is said in the spec, its value only at launch:
    // the spec holds nothing secret, and the sandbox only ever sees a
    // placeholder, which the proxy replaces in the requests to the API.
    let mut keys = Keys::new();
    if let Ok(key) = std::env::var(KEY_ENV) {
        spec = spec.with_key(KeySpec::for_service(KEY_ENV, KEY_SERVICE));
        keys = keys.with_key(KEY_ENV, key);
    } else {
        eprintln!("no {KEY_ENV}: the agent relies on the credential the daemon holds for its kit");
    }
    (spec, keys)
}

/// Runs the task in the sandbox of `lease`, then checks the work.
async fn work(lease: &Lease, tools: &HttpServer, project: &str, original: &str) -> Result<()> {
    // The token the launch drew for the tools of this sandbox: from now on,
    // the tools answer the agent of this sandbox, and nobody else.
    let token = lease
        .mcp_token(TOOLS)
        .context("the launch draws a token for a server of the host")?;
    tools.rotate_token(token.expose_secret());

    // The tests fail before the agent: that is the task.
    let before = automation::tests(lease.sandbox(), project).await?;
    eprintln!("tests before the agent: exit code {}", before.exit_code);

    run_agent(lease, project).await?;
    check(lease, tools, project, original).await
}

/// Step 4, the task: the agent of the kit, run without a terminal on the
/// project.
async fn run_agent(lease: &Lease, project: &str) -> Result<()> {
    // The command comes from the profile of the agent: its program and the
    // options its sessions start it with, from its kit
    // (`claude --dangerously-skip-permissions`), then the prompt and every
    // step as a JSON event, as the documentation of Claude Code says.
    let headless = Agent::Claude
        .headless(lease.agent().context("the kit declares no agent")?, PROMPT)
        .context("the agent runs no prompt without a terminal")?;
    eprintln!("step 4: the agent works on the task");
    let spec = AttachSpec::new(headless.command).with_working_dir(project);
    let code = tokio::time::timeout(
        AGENT_DEADLINE,
        automation::run_agent(lease.sandbox(), &spec, headless.output),
    )
    .await
    .context("the agent did not finish within its deadline")??;
    eprintln!("the agent exited with code {code}");
    Ok(())
}

/// Step 5: the work of the agent, checked by the program itself.
async fn check(lease: &Lease, tools: &HttpServer, project: &str, original: &str) -> Result<()> {
    eprintln!("step 5: checking the work");
    let sandbox = lease.sandbox();

    // The tests of the project decide, not what the agent says.
    let after = automation::tests(sandbox, project).await?;
    print!("{}", after.stdout);

    // What the agent changed, against the copy it could not touch.
    let diff = sandbox
        .exec(&ExecSpec::new(["diff", "-ru", original, project]))
        .await?;
    println!("changes made by the agent:\n{}", diff.stdout);

    // Where the sandbox went, as its proxy saw it.
    automation::report_network(sandbox).await?;

    // The tools of the host saw the agent, with the token of this sandbox.
    println!(
        "the tools of the host received {} requests with the token of the sandbox, {} without",
        tools.authorized(),
        tools.refused()
    );

    // What the launch did: the kits, the rules, the keys, the MCP servers and
    // the credentials the daemon withholds. It never holds a key or a token,
    // so it can go to a log.
    println!(
        "the report of the launch: {}",
        serde_json::to_string(lease.report())?
    );

    after
        .check()
        .context("the tests still fail after the agent")?;
    ensure!(
        tools.authorized() > 0,
        "the agent never called the tools of the host"
    );
    println!("the task is done: the tests pass, and the agent used the tools of the host");
    Ok(())
}
