//! The arguments of the `claude` kit chosen without the `sbx` CLI: the same
//! sandbox as [`agent-kit-args`](../../agent-kit-args/src/main.rs), from a
//! package forged through the socket alone.
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
//! The creation reports the credential the kit declares, `anthropic`, as
//! withheld: no bindings file authorizes it, so the daemon gives the sandbox
//! none of its values. The example never starts Claude, and uses no
//! credential.
//!
//! ```sh
//! just example agent-kit-forge
//! ```

use anyhow::{Context, Result, ensure};
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::kit::KitForge;
use example_common::{connect, sandbox};
use serde_json::Value;

/// The published kit, rebuilt from its image.
const BASE: &str = "docker.io/sbx/claude:latest";

/// Where the package is written.
const OUTPUT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/forged/agent-kit-forge"
);

/// The model chosen for the sandbox.
const MODEL: &str = "claude-haiku-5-5";

/// The effort chosen for the sandbox.
const EFFORT: &str = "low";

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    eprintln!(
        "forging the package: {BASE} rebuilt from its image, with model={MODEL} and reasoningEffort={EFFORT}"
    );
    let package = KitForge::new(client.clone())
        .with_base(BASE)
        .with_arg("model", MODEL)
        .with_arg("reasoningEffort", EFFORT)
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
        Ok(())
    })
    .await
}

/// The names of the argument declarations `args`, comma-separated.
fn names(args: &Value) -> String {
    args.as_object()
        .map(|declared| declared.keys().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}
