//! Devin run unattended: one prompt, no terminal, and a verification that
//! does not trust the agent.
//!
//! The example writes a small project whose test suite fails under
//! `examples/target/automation/`, mounts it as the workspace of a throw-away
//! sandbox, runs the agent in its non-interactive mode with the prompt on the
//! files of the host, then runs the tests again and shows the diff and the
//! hosts the agent reached.
//! A prompt given on the command line replaces the default task. It exits
//! with an error when the tests still fail.
//!
//! The command comes from the [documentation of the agent][docs]; the
//! program and its run options come from the kit itself.
//!
//! Credentials, required since nobody can log in: `DEVIN_API_KEY`, given to the
//! `devin` service of `sbx`.
//! A secret stored with `sbx secret set` serves as well.
//!
//! ```sh
//! just example agent-devin-automation
//! just example agent-devin-automation "Add a div operation to calc.sh, with its tests"
//! ```
//!
//! [docs]: https://docs.devin.ai/cli/reference/commands

use docker_sandbox::kit::KitAgent;
use example_common::automation::{self, Headless, Output};

/// The non-interactive command of Devin for `prompt`.
fn command(agent: &KitAgent, prompt: &str) -> Vec<String> {
    // `-p` runs one prompt and exits; the run options of the kit
    // (`--permission-mode dangerous --respect-workspace-trust=false`) let it
    // act without approval in this directory.
    agent.command(["-p", prompt])
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let headless = Headless {
        command,
        output: Output::Text,
    };
    automation::run(&example_common::agent::DEVIN, headless).await
}
