//! The MCP servers of a spec: registered in its agent by a mixin, their
//! tokens set in the sandbox.
//!
//! The servers are the [`McpServer`]s of `docker-sandbox-kit`, registered by
//! `Mixin::mcp` in claude, copilot or opencode: a spec that gives them
//! servers names its agent with `agent`, since the agent of a published base
//! kit is known only once its kit is forged. A server of the host reads its
//! token from a variable the spec sets; a remote server reads its own from a
//! variable a key or `env` sets, and the token of a server of the host is
//! drawn at each launch (`token.rs`).

mod token;

pub(crate) use self::token::Tokens;

use docker_sandbox_kit::{McpServer, Mixin};
use serde::Serialize;

use crate::error::{Error, Result, invalid};
use crate::spec::{Owners, SandboxSpec};

/// An MCP server registered in the agent, as the report of a launch shows
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ServerReport {
    /// Name the agent knows it under.
    pub name: String,
    /// Its kind, as its JSON form names it: `host`, `remote` or `stdio`.
    pub kind: String,
    /// The agent it is registered in.
    pub agent: String,
}

impl SandboxSpec {
    /// Registers the MCP server `server` in the agent.
    #[must_use]
    pub fn with_mcp(mut self, server: McpServer) -> Self {
        self.mcp.push(server);
        self
    }

    /// The mixin of the MCP servers of the spec, if it has any.
    pub(crate) fn mcp_mixin(&self) -> Result<Option<Mixin>> {
        if self.mcp.is_empty() {
            return Ok(None);
        }
        let Some(agent) = self.agent else {
            return Err(invalid(
                "mcp: MCP servers are registered in an agent, named by agent rather than base",
            ));
        };
        Mixin::mcp(agent, self.mcp.clone())
            .map(Some)
            .map_err(Error::from)
    }

    /// The servers of the host and the variable each reads its token from.
    pub(crate) fn host_tokens(&self) -> impl Iterator<Item = (&McpServer, &str)> {
        self.mcp
            .iter()
            .filter(|server| server_kind(server) == "host")
            // A server of the host always reads its token from a variable.
            .filter_map(|server| server.token_env().map(|env| (server, env)))
    }

    /// Checks that the token of every remote server is set by one of
    /// `owners`, a key or `env`.
    pub(crate) fn check_remote_tokens(&self, owners: &Owners<'_>) -> Result<()> {
        for server in self
            .mcp
            .iter()
            .filter(|server| server_kind(server) == "remote")
        {
            if let Some(env) = server.token_env()
                && !owners.contains_key(env)
            {
                return Err(invalid(format!(
                    "mcp: the server {} reads its token from {env}, which neither a key nor env \
                     sets",
                    server.name()
                )));
            }
        }
        Ok(())
    }

    /// The MCP servers of the spec, as the report of a launch shows them.
    pub(crate) fn server_reports(&self) -> Vec<ServerReport> {
        let agent = self
            .agent
            .map(|agent| agent.name().to_owned())
            .unwrap_or_default();
        self.mcp
            .iter()
            .map(|server| ServerReport {
                name: server.name().to_owned(),
                kind: server_kind(server),
                agent: agent.clone(),
            })
            .collect()
    }
}

/// The kind of `server`, as its JSON form names it: `host`, `remote` or
/// `stdio`.
pub(crate) fn server_kind(server: &McpServer) -> String {
    serde_json::to_value(server)
        .ok()
        .and_then(|form| form["kind"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use docker_sandbox_kit::Agent;

    use super::*;
    use crate::spec::broken;

    /// A claude spec.
    fn claude() -> SandboxSpec {
        SandboxSpec::agent(Agent::Claude)
    }

    #[test]
    fn servers_are_registered_in_an_agent_that_has_an_adapter() {
        let base = SandboxSpec::base("docker.io/sbx/claude:latest")
            .with_mcp(McpServer::host("tools", 47813));
        assert!(broken(&base).starts_with("mcp: MCP servers are registered in an agent"));
        let shell = SandboxSpec::agent(Agent::Shell).with_mcp(McpServer::host("tools", 47813));
        let error = shell.prepare().unwrap_err().to_string();
        assert!(error.contains("shell has no MCP adapter"), "{error}");
        assert!(claude().mcp_mixin().unwrap().is_none());
    }

    #[test]
    fn remote_servers_read_a_token_something_sets() {
        let remote =
            McpServer::remote("docs", "https://mcp.example.com/mcp").with_bearer_env("DOCS_TOKEN");
        let unset = claude().with_mcp(remote.clone());
        assert!(broken(&unset).contains("reads its token from DOCS_TOKEN"));
        claude()
            .with_mcp(remote)
            .with_env("DOCS_TOKEN", "a dummy token")
            .prepare()
            .unwrap();
        let kinds: Vec<String> = [
            McpServer::host("tools", 47813),
            McpServer::remote("docs", "https://mcp.example.com/mcp"),
            McpServer::stdio("files", "mcp-fs"),
        ]
        .iter()
        .map(server_kind)
        .collect();
        assert_eq!(kinds, ["host", "remote", "stdio"]);
    }
}
