//! OpenCode in a throw-away sandbox, through the socket alone.
//!
//! The kit of the agent is compiled once with the `sbx` CLI, a sandbox is
//! created from it, the credentials found in the environment are given to
//! that sandbox only, and the terminal is plugged into the agent session. The
//! sandbox is removed when the session ends.
//!
//! Credentials: the key of any model provider: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, `GROQ_API_KEY`, `MISTRAL_API_KEY`, `NEBIUS_API_KEY` or `XAI_API_KEY`. It reaches the agent as a placeholder the proxy replaces; without any, the credential the daemon holds for the kit serves, or log in inside the session.
//!
//! ```sh
//! just example agent-opencode
//! ```

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    example_common::agent::run(&example_common::agent::OPENCODE, false).await
}
