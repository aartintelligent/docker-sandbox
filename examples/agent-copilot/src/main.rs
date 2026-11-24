//! GitHub Copilot in a throw-away sandbox, through the socket alone.
//!
//! The kit of the agent is compiled once with the `sbx` CLI, a sandbox is
//! created from it, the credentials found in the environment are given to
//! that sandbox only, and the terminal is plugged into the agent session. The
//! sandbox is removed when the session ends.
//!
//! Credentials: a GitHub token in `COPILOT_GITHUB_TOKEN`, `GH_TOKEN` or `GITHUB_TOKEN`. A secret stored with `sbx secret set` serves as well; without any, log in inside the session.
//!
//! ```sh
//! just example agent-copilot
//! ```

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    example_common::agent::run(&example_common::agent::COPILOT, false).await
}
