//! Codex run unattended: one prompt, no terminal, and a verification that
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
//! The command is the profile of the agent in `docker-sandbox-kit`
//! (`Agent::headless`), from the [documentation of the agent][docs]; the
//! program and its run options come from the kit itself.
//!
//! Credentials, required since nobody can log in: `OPENAI_API_KEY` or
//! `CODEX_API_KEY`.
//! The key reaches the agent through the proxy of the sandbox alone, as a
//! placeholder ([`credentials`](../../common/src/credentials.rs)).
//!
//! ```sh
//! just example agent-codex-automation
//! just example agent-codex-automation "Add a div operation to calc.sh, with its tests"
//! ```
//!
//! [docs]: https://learn.chatgpt.com/docs/non-interactive-mode

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    example_common::automation::run(&example_common::agent::CODEX).await
}
