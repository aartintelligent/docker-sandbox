//! The tokens of the MCP servers of the host, drawn at each launch.
//!
//! A server of the host checks the bearer the agent sends: the launch draws
//! a token for each, 32 bytes of `/dev/urandom` in hexadecimal, sets it in
//! the variable the agent reads it from, and gives it to the program through
//! the lease. The token is the sandbox's own, and in clear inside it: a
//! process of the sandbox can read it, a process elsewhere cannot guess it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use docker_sandbox::sandbox::Secret;

use crate::error::Result;
use crate::spec::SandboxSpec;

/// Source of the tokens.
const RANDOM: &str = "/dev/urandom";

/// Bytes of a token, written in hexadecimal.
const TOKEN_BYTES: usize = 32;

/// The tokens of a launch, by server: the variable each goes in, and the
/// token.
pub(crate) type Tokens = BTreeMap<String, (String, Secret)>;

impl SandboxSpec {
    /// A token for each MCP server of the host, with the variable it goes
    /// in, by server.
    pub(crate) fn draw_tokens(&self) -> Result<Tokens> {
        let mut tokens = Tokens::new();
        for (server, env) in self.host_tokens() {
            tokens.insert(server.name().to_owned(), (env.to_owned(), token()?));
        }
        Ok(tokens)
    }
}

/// A token of [`TOKEN_BYTES`] random bytes, in hexadecimal.
fn token() -> Result<Secret> {
    token_from(Path::new(RANDOM))
}

/// A token of [`TOKEN_BYTES`] bytes read from `source`, in hexadecimal.
fn token_from(source: &Path) -> Result<Secret> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    File::open(source)
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|error| docker_sandbox::Error::File {
            path: source.to_owned(),
            source: error,
        })?;
    let mut text = String::with_capacity(2 * TOKEN_BYTES);
    for byte in bytes {
        // Writing to a `String` cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    Ok(Secret::new(text))
}

#[cfg(test)]
mod tests {
    use docker_sandbox_kit::{Agent, McpServer};

    use super::*;

    #[test]
    fn tokens_are_random_hexadecimal_and_their_source_is_named() {
        let (one, two) = (token().unwrap(), token().unwrap());
        assert_eq!(one.expose_secret().len(), 2 * TOKEN_BYTES);
        assert_ne!(one.expose_secret(), two.expose_secret());
        let error = token_from(Path::new("/nonexistent/random")).unwrap_err();
        assert!(error.to_string().contains("/nonexistent/random"), "{error}");
    }

    #[test]
    fn servers_of_the_host_are_drawn_a_token_each() {
        let tokens = SandboxSpec::agent(Agent::Claude)
            .with_mcp(McpServer::host("tools", 47813))
            .with_mcp(McpServer::stdio("files", "mcp-fs"))
            .draw_tokens()
            .unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens["tools"].0, "DOCKER_SANDBOX_MCP_TOOLS_TOKEN");
    }
}
