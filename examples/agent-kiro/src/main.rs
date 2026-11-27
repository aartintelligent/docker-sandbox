//! Kiro in a throw-away sandbox, through the socket alone.
//!
//! The kit of the agent is forged without the `sbx` CLI, a sandbox is
//! created from it, the credentials found in the environment are given to
//! that sandbox only, and the terminal is plugged into the agent session. The
//! sandbox is removed when the session ends.
//!
//! Credentials: none `sbx` can inject for Kiro: sign in with the device flow the agent offers on first run, or set `KIRO_API_KEY`, given to the agent process as is.
//!
//! ```sh
//! just example agent-kiro
//! ```

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    example_common::agent::run(&example_common::agent::KIRO, false).await
}
