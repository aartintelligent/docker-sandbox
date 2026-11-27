//! Running a coding agent unattended: a task, a prompt, and a verification
//! that does not trust the agent.
//!
//! [`run`] is the whole flow of the `agent-*-automation` examples, the flow of
//! a runner: compile the kit once, create a sandbox with the placeholder of
//! the key found in the environment and give the key to its proxy, mount the
//! task written on the host as its workspace, run the
//! agent in its non-interactive mode with a deadline, then check the result
//! with the project's own tests, and show what changed and which hosts the
//! agent reached. The agent works on the files of the host, as with `sbx run`:
//! nothing is copied back.
//!
//! The default task is the `calc` project of `common/task/`, whose test suite
//! fails; a prompt given on the command line replaces the default one.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use docker_sandbox::exec::{AttachSpec, ExecOutput, ExecSpec, OutputStream};
use docker_sandbox::kit::KitAgent;
use docker_sandbox::sandbox::{CreateRequest, Sandbox, Workspace};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::agent::Agent;
use crate::{connect, credentials, kit, sandbox};

/// Files of the default task, written on the host.
const TASK: &[(&str, &str, u32)] = &[
    ("calc.sh", include_str!("../task/calc.sh"), 0o755),
    ("test.sh", include_str!("../task/test.sh"), 0o755),
    ("README.md", include_str!("../task/README.md"), 0o644),
];

/// Prompt of the default task.
pub const DEFAULT_PROMPT: &str = "The test suite of this project fails: run `sh test.sh` to see \
     it. Fix the bug in calc.sh so that every test passes, without changing test.sh. Then run \
     `sh test.sh` again to confirm, and summarize the fix in one sentence.";

/// How long the agent may work on the task.
const AGENT_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// How long the exit code of the agent is awaited once its output has ended.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest summary printed for one streamed event.
const SUMMARY_CHARS: usize = 300;

/// Where the task of each run is written, in the target directory of the
/// examples.
const RESULTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/automation");

/// How an agent runs one prompt without a terminal.
#[derive(Debug, Clone, Copy)]
pub struct Headless {
    /// The command running `prompt`, built from the program of the agent and
    /// the run options its kit declares.
    pub command: fn(agent: &KitAgent, prompt: &str) -> Vec<String>,
    /// What the command prints on its standard output.
    pub output: Output,
}

/// The program of `agent`, or `fallback` when its kit declares none: the
/// start of a command that puts a subcommand before its options.
#[must_use]
pub fn program(agent: &KitAgent, fallback: &str) -> String {
    agent.binary.clone().unwrap_or_else(|| fallback.to_owned())
}

/// Format of the standard output of an agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Text, relayed as it comes.
    Text,
    /// One JSON event per line, summarized as it comes: the step, the text of
    /// a message, the tool called, the final result.
    JsonLines,
}

/// Runs `agent` unattended on the default task, or on the prompt given on the
/// command line.
///
/// # Errors
///
/// Fails when the task cannot be written or the sandbox prepared, the agent
/// exceeds its deadline (an agent the daemon gives no credential cannot log
/// in), or the
/// tests of the task still fail afterwards. The sandbox is removed in every
/// case; the task stays on the host.
pub async fn run(agent: &Agent, headless: Headless) -> Result<()> {
    let prompt = prompt_from_args();
    let forwarded = agent.forwarded_values();
    if credentials::find_in_env(agent.credentials).is_empty() && forwarded.is_empty() {
        eprintln!(
            "no key in the environment ({}): the agent relies on the credential the daemon \
             holds for its kit, a subscription for instance; unattended, it cannot log in",
            env_names(agent)
        );
    }
    let client = connect().await?;
    let package = kit::package(&client, agent.kit).await?;
    let described = package
        .agent()
        .context("the kit declares no agent program")?;
    let name = sandbox::unique_name(&format!("{}-auto", agent.kit));

    // The task lives on the host, mounted as the workspace of the sandbox at
    // its own path: the agent changes the files of the host. A pristine copy,
    // mounted read-only, gives the diff without the agent being able to alter
    // it.
    let run_dir = PathBuf::from(RESULTS).join(&name);
    let (project, original) = (run_dir.join("calc"), run_dir.join("calc.orig"));
    write_task(&project)?;
    write_task(&original)?;
    let (project, original) = (canonical(&project)?, canonical(&original)?);
    let keys = credentials::proxied_keys(&package, agent.credentials)?;
    let request = keys.iter().fold(
        package
            .request(&name)?
            .with_workspace(&project)
            .with_additional_workspace(Workspace::new(&original).read_only()),
        CreateRequest::with_proxied_key,
    );
    eprintln!("task written to {project}, mounted as the workspace");

    sandbox::with_sandbox_from(&client, &package, &request, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        if !keys.is_empty() {
            sandbox.push_proxied_keys(&keys).await?;
            eprintln!(
                "keys given to the proxy of the sandbox: {}",
                credentials::describe(&keys)
            );
        }

        let before = tests(&sandbox, &project).await?;
        eprintln!("tests before the agent: exit code {}", before.exit_code);

        let command = (headless.command)(&described, &prompt);
        eprintln!("running {} without a terminal", command.join(" "));
        let spec = AttachSpec::new(command)
            .with_working_dir(&project)
            .with_envs(forwarded);
        let code =
            tokio::time::timeout(AGENT_DEADLINE, run_agent(&sandbox, &spec, headless.output))
                .await
                .context("the agent did not finish within its deadline")??;
        eprintln!("agent exited with code {code}");

        let after = tests(&sandbox, &project).await?;
        print!("{}", after.stdout);
        let diff = sandbox
            .exec(&ExecSpec::new(["diff", "-ru", &original, &project]))
            .await?;
        println!("changes made by the agent:\n{}", diff.stdout);
        eprintln!("the project, as the agent left it: {project}");
        report_network(&sandbox).await?;

        after
            .check()
            .map(drop)
            .context("the tests still fail after the agent")
    })
    .await
}

/// The prompt given on the command line, or [`DEFAULT_PROMPT`].
fn prompt_from_args() -> String {
    let words: Vec<String> = std::env::args().skip(1).collect();
    if words.is_empty() {
        DEFAULT_PROMPT.to_owned()
    } else {
        words.join(" ")
    }
}

/// Writes the default task into the host directory `dir`, created when
/// missing.
fn write_task(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
    for (file, contents, mode) in TASK {
        let file = dir.join(file);
        std::fs::write(&file, contents).with_context(|| file.display().to_string())?;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(*mode))?;
    }
    Ok(())
}

/// The real path of `path`, as a workspace must be given: it is mounted at
/// that path, and a `..` in it (`common/../target`) names nothing inside the
/// sandbox.
fn canonical(path: &Path) -> Result<String> {
    let real = std::fs::canonicalize(path).with_context(|| path.display().to_string())?;
    Ok(real.display().to_string())
}

/// Runs the test suite of the task.
async fn tests(sandbox: &Sandbox, project: &str) -> Result<ExecOutput> {
    Ok(sandbox
        .exec(&ExecSpec::new(["sh", "test.sh"]).with_working_dir(project))
        .await?)
}

/// Runs `spec` without a terminal, relaying its output as it comes, and
/// returns its exit code.
async fn run_agent(sandbox: &Sandbox, spec: &AttachSpec, format: Output) -> Result<i32> {
    let (mut input, mut output, control) = sandbox.attach(spec).await?.split();
    // Nothing is typed: closing the input keeps the agent from waiting for it.
    input.shutdown().await?;
    let (mut stdout, mut stderr) = (tokio::io::stdout(), tokio::io::stderr());
    let mut pending = Vec::new();
    while let Some(chunk) = output.next().await {
        let chunk = chunk?;
        match (chunk.stream, format) {
            (OutputStream::Stdout, Output::Text) => stdout.write_all(&chunk.data).await?,
            (OutputStream::Stdout, Output::JsonLines) => {
                pending.extend_from_slice(&chunk.data);
                while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    if let Some(summary) = summarize(&line) {
                        stdout.write_all(summary.as_bytes()).await?;
                    }
                }
            }
            (OutputStream::Stderr, _) => stderr.write_all(&chunk.data).await?,
        }
        stdout.flush().await?;
    }
    if let Some(summary) = summarize(&pending) {
        stdout.write_all(summary.as_bytes()).await?;
    }
    Ok(control.wait(EXIT_TIMEOUT).await?)
}

/// One line summarizing a JSON event of an agent, or the line itself when it
/// is not JSON. The fields read are those the agents document for their
/// streamed output: `type` and `subtype`, a `message` with content blocks of
/// `text` or `tool_use`, an `item` with a `text`, and the final `result`.
fn summarize(line: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let Ok(event) = serde_json::from_str::<Value>(text) else {
        return Some(format!("{text}\n"));
    };
    let kind = event["type"].as_str().unwrap_or("event");
    let mut parts = Vec::new();
    if let Some(subtype) = event["subtype"].as_str() {
        parts.push(subtype.to_owned());
    }
    if let Some(blocks) = event.pointer("/message/content").and_then(Value::as_array) {
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => parts.extend(block["text"].as_str().map(str::to_owned)),
                Some("tool_use") => {
                    parts.push(format!("tool {}", block["name"].as_str().unwrap_or("?")));
                }
                _ => {}
            }
        }
    }
    for pointer in ["/item/text", "/text", "/result"] {
        if let Some(value) = event.pointer(pointer).and_then(Value::as_str) {
            parts.push(value.to_owned());
        }
    }
    let summary: String = parts
        .join(" · ")
        .replace('\n', " ")
        .chars()
        .take(SUMMARY_CHARS)
        .collect();
    Some(format!("[{kind}] {summary}\n"))
}

/// Prints the hosts the sandbox reached or was refused, as the proxy logged
/// them.
async fn report_network(sandbox: &Sandbox) -> Result<()> {
    let log = sandbox
        .client()
        .policy()
        .log()
        .await?
        .for_sandbox(sandbox.name());
    for host in &log.allowed_hosts {
        eprintln!(
            "network: reached {} ({} times)",
            host.host, host.count_since
        );
    }
    for host in &log.blocked_hosts {
        eprintln!(
            "network: refused {} ({} times)",
            host.host, host.count_since
        );
    }
    Ok(())
}

/// Every environment variable an agent can take a credential from, for an
/// error message.
fn env_names(agent: &Agent) -> String {
    agent
        .credentials
        .iter()
        .flat_map(|credential| credential.env.iter().copied())
        .chain(agent.forwarded_env.iter().copied())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_events_are_summarized() {
        let assistant = br#"{"type":"assistant","message":{"content":[{"type":"text","text":"Running the tests"},{"type":"tool_use","name":"Bash"}]}}"#;
        assert_eq!(
            summarize(assistant).as_deref(),
            Some("[assistant] Running the tests · tool Bash\n")
        );
        let result = br#"{"type":"result","subtype":"success","result":"Fixed sub."}"#;
        assert_eq!(
            summarize(result).as_deref(),
            Some("[result] success · Fixed sub.\n")
        );
        assert_eq!(summarize(b"plain text").as_deref(), Some("plain text\n"));
        assert_eq!(summarize(b"  \n"), None);
    }
}
