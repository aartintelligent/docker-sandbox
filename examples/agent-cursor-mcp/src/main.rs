//! Cursor in a throw-away sandbox, with the MCP server of the examples.
//!
//! The example registers itself as a local MCP server (`sbx mcp add`), loads
//! it into the gateway of the sandbox, and starts the agent session: ask the
//! agent to call the `sum`, `echo` or `host_time` tool. The sandbox and the
//! registration are removed when the session ends. When `sbx` starts this
//! binary as the MCP server, it serves MCP instead of running the example.
//!
//! Credentials: a Cursor key in `CURSOR_API_KEY`. It reaches the agent as a placeholder the proxy replaces; without any, the credential the daemon holds for the kit serves, or log in inside the session.
//!
//! ```sh
//! just example agent-cursor-mcp
//! ```

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    example_common::agent::run(&example_common::agent::CURSOR, true).await
}
