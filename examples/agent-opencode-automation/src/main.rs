//! OpenCode run unattended: one prompt, no terminal, and a verification that
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
//! Credentials, required since nobody can log in: the key of any model provider
//! (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …).
//! A secret stored with `sbx secret set` serves as well.
//!
//! ```sh
//! just example agent-opencode-automation
//! just example agent-opencode-automation "Add a div operation to calc.sh, with its tests"
//! ```
//!
//! [docs]: https://opencode.ai/docs/cli/

use docker_sandbox::kit::KitAgent;
use example_common::automation::{self, Headless, Output, program};

/// The non-interactive command of OpenCode for `prompt`.
fn command(agent: &KitAgent, prompt: &str) -> Vec<String> {
    // `run` runs one prompt and exits; `--auto` approves every permission
    // that is not explicitly denied.
    vec![
        program(agent, "opencode"),
        "run".to_owned(),
        "--auto".to_owned(),
        prompt.to_owned(),
    ]
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let headless = Headless {
        command,
        output: Output::Text,
    };
    automation::run(&example_common::agent::OPENCODE, headless).await
}
