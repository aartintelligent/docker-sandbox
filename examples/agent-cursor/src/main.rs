//! Cursor in a throw-away sandbox, through the socket alone.
//!
//! The kit of the agent is compiled once with the `sbx` CLI, a sandbox is
//! created from it, the credentials found in the environment are given to
//! that sandbox only, and the terminal is plugged into the agent session. The
//! sandbox is removed when the session ends.
//!
//! Credentials: a Cursor key in `CURSOR_API_KEY`. It reaches the agent as a placeholder the proxy replaces; without any, log in inside the session.
//!
//! ```sh
//! just example agent-cursor
//! ```

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    example_common::agent::run(&example_common::agent::CURSOR, false).await
}
