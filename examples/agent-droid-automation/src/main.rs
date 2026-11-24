//! Droid run unattended: one prompt, no terminal, and a verification that
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
//! Credentials, required since nobody can log in: `FACTORY_API_KEY`.
//! A secret stored with `sbx secret set` serves as well.
//!
//! ```sh
//! just example agent-droid-automation
//! just example agent-droid-automation "Add a div operation to calc.sh, with its tests"
//! ```
//!
//! [docs]: https://docs.factory.ai/cli/droid-exec/overview

use docker_sandbox::kit::KitAgent;
use example_common::automation::{self, Headless, Output, program};

/// The non-interactive command of Droid for `prompt`.
fn command(agent: &KitAgent, prompt: &str) -> Vec<String> {
    // `exec` runs one prompt and exits; it is read-only by default, `--auto
    // high` lets it edit and run commands, and `stream-json` reports every step.
    vec![
        program(agent, "droid"),
        "exec".to_owned(),
        "--auto".to_owned(),
        "high".to_owned(),
        "-o".to_owned(),
        "stream-json".to_owned(),
        prompt.to_owned(),
    ]
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let headless = Headless {
        command,
        output: Output::JsonLines,
    };
    automation::run(&example_common::agent::DROID, headless).await
}
