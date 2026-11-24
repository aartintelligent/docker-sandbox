//! OpenCode in a throw-away sandbox, with the MCP server of the examples.
//!
//! The example registers itself as a local MCP server (`sbx mcp add`), loads
//! it into the gateway of the sandbox, and starts the agent session: ask the
//! agent to call the `sum`, `echo` or `host_time` tool. The sandbox and the
//! registration are removed when the session ends. When `sbx` starts this
//! binary as the MCP server, it serves MCP instead of running the example.
//!
//! Credentials: the key of any model provider: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, `GROQ_API_KEY`, `MISTRAL_API_KEY`, `NEBIUS_API_KEY` or `XAI_API_KEY`. A secret stored with `sbx secret set` serves as well; without any, log in inside the session.
//!
//! ```sh
//! just example agent-opencode-mcp
//! ```

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    example_common::agent::run(&example_common::agent::OPENCODE, true).await
}
