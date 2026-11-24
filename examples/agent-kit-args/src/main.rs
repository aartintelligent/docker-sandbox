//! Arguments a kit declares, chosen when the kit is compiled: the model and
//! the effort of the `claude` kit, as `sbx create --kit-arg model=…` sets
//! them.
//!
//! A kit declares its arguments with a default, a description and a pattern
//! or a list of values; the `claude` kit declares `model` and
//! `reasoningEffort`. The CLI resolves their values into the kit artifact
//! when it compiles the kit (`CompileJob::with_arg`): the sandbox gets them as
//! environment variables, and the install command of the kit writes them into
//! `~/.claude/settings.json`. A program that offers a choice of model compiles
//! one package per choice, once, and creates its sandboxes from it.
//!
//! The example creates the sandbox and reads both, without starting Claude:
//! no credential is used.
//!
//! ```sh
//! just example agent-kit-args
//! ```

use anyhow::{Context, Result, ensure};
use docker_sandbox::exec::ExecSpec;
use example_common::{connect, kit, sandbox};
use serde_json::Value;

/// The model chosen for the sandbox.
const MODEL: &str = "claude-haiku-5-5";

/// The effort chosen for the sandbox.
const EFFORT: &str = "low";

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package_with_args(
        &client,
        "claude",
        &[("model", MODEL), ("reasoningEffort", EFFORT)],
    )
    .await?;
    let name = sandbox::unique_name("kit-args");
    sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
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
        Ok(())
    })
    .await
}
