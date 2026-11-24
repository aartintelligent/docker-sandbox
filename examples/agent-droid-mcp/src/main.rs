//! Droid in a throw-away sandbox, with the MCP server of the examples.
//!
//! The example registers itself as a local MCP server (`sbx mcp add`), loads
//! it into the gateway of the sandbox, and starts the agent session: ask the
//! agent to call the `sum`, `echo` or `host_time` tool. The sandbox and the
//! registration are removed when the session ends. When `sbx` starts this
//! binary as the MCP server, it serves MCP instead of running the example.
//!
//! Credentials: a Factory key in `FACTORY_API_KEY`. A secret stored with `sbx secret set` serves as well; without any, log in inside the session.
//!
//! ```sh
//! just example agent-droid-mcp
//! ```

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    example_common::agent::run(&example_common::agent::DROID, true).await
}
