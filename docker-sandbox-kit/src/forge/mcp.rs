//! MCP servers given to the agent of a sandbox by a mixin, with no file on
//! the host.
//!
//! The daemon only loads into the gateway of a sandbox the servers registered
//! with `sbx mcp add`, which writes a file of the state of the daemon. A mixin
//! registers a server in the configuration of the agent instead, inside the
//! sandbox and at install time, as the kits of the agents register their
//! gateway: `claude mcp add` for claude. The server is served by a process of
//! the host, on its loopback ([`McpServer::host`]), or remote
//! ([`McpServer::remote`]). Its bearer token is read from a variable the
//! creation request sets, so that one forged package serves sandboxes with
//! tokens of their own.

use super::Mixin;
use super::descriptor::{KitDescriptor, LifecycleStep, is_variable_name};
use crate::Agent;
use crate::error::{Error, Result};

/// The host the sandbox reaches the loopback of the host under.
const HOST: &str = "host.docker.internal";

/// The path of the MCP endpoint of a server served by the host.
const HOST_PATH: &str = "/mcp";

/// The name the kits of the agents register the gateway of the sandbox
/// under, which a server of a mixin cannot take.
const GATEWAY: &str = "mcp-gateway";

/// An MCP server for the agent of a sandbox, registered by [`Mixin::mcp`].
///
/// ```
/// use docker_sandbox_kit::McpServer;
///
/// let tools = McpServer::host("project-tools", 47813);
/// assert_eq!(tools.url(), "http://host.docker.internal:47813/mcp");
/// assert_eq!(tools.token_env(), Some("DOCKER_SANDBOX_MCP_PROJECT_TOOLS_TOKEN"));
///
/// let linear = McpServer::remote("linear", "https://mcp.linear.app/mcp").with_bearer_env("LINEAR_TOKEN");
/// assert_eq!(linear.token_env(), Some("LINEAR_TOKEN"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// Name the agent knows the server under.
    name: String,
    /// URL of its endpoint, streamable HTTP.
    url: String,
    /// Variable of the sandbox holding its bearer token, if it takes one.
    token_env: Option<String>,
}

impl McpServer {
    /// A server served by a process of the host, on `port` of its loopback,
    /// in any language: the sandbox reaches it at
    /// `http://host.docker.internal:<port>/mcp`. Its bearer token is read
    /// from the variable [`McpServer::token_env`]
    /// (`DOCKER_SANDBOX_MCP_<NAME>_TOKEN`), which the creation request sets
    /// and the server checks. The proxy rewrites no request to the host: the
    /// sandbox holds that token, which is to be random and its own.
    pub fn host(name: impl Into<String>, port: u16) -> Self {
        let name = name.into();
        let token_env = format!(
            "DOCKER_SANDBOX_MCP_{}_TOKEN",
            name.to_ascii_uppercase().replace('-', "_")
        );
        Self {
            url: format!("http://{HOST}:{port}{HOST_PATH}"),
            name,
            token_env: Some(token_env),
        }
    }

    /// A remote server at `url`, called by the agent itself, without a token
    /// until [`McpServer::with_bearer_env`].
    pub fn remote(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            token_env: None,
        }
    }

    /// Sends the content of the variable `env` of the sandbox as the bearer
    /// token of the server. For a key the sandbox must not hold, set `env` to
    /// the placeholder of a `ProxiedKey` for the host of the server: the
    /// proxy writes the key in its place.
    #[must_use]
    pub fn with_bearer_env(mut self, env: impl Into<String>) -> Self {
        self.token_env = Some(env.into());
        self
    }

    /// Name the agent knows the server under.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// URL of the endpoint of the server.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Variable of the sandbox holding the bearer token of the server, if it
    /// takes one.
    #[must_use]
    pub fn token_env(&self) -> Option<&str> {
        self.token_env.as_deref()
    }

    /// The host of the URL, with its port when it has one: what the network
    /// rule of the mixin allows.
    fn authority(&self) -> &str {
        let rest = self.url.split_once("://").map_or("", |(_, rest)| rest);
        rest.split(['/', '?', '#']).next().unwrap_or_default()
    }

    /// What is wrong with the server, if anything.
    fn check(&self) -> std::result::Result<(), String> {
        let name = &self.name;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "{name:?} is not a name of MCP server: letters, digits, - and _"
            ));
        }
        if name == GATEWAY {
            return Err(format!(
                "{GATEWAY} is the name the kit registers the gateway of the sandbox under"
            ));
        }
        if !(self.url.starts_with("http://") || self.url.starts_with("https://")) {
            return Err(format!("the URL of {name} is not an http or https one"));
        }
        if self
            .url
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '\\' | '`' | '$'))
        {
            return Err(format!(
                "the URL of {name} holds a character its command cannot carry"
            ));
        }
        if self.authority().is_empty() {
            return Err(format!("the URL of {name} names no host"));
        }
        match self.token_env() {
            Some(env) if !is_variable_name(env) => Err(format!(
                "{env:?} is not the name of an environment variable"
            )),
            _ => Ok(()),
        }
    }

    /// The command that registers the server in Claude Code, for the user
    /// `agent`: the form the claude kit registers its gateway in.
    fn claude_command(&self) -> String {
        let header = self.token_env().map_or_else(String::new, |env| {
            format!(" --header \"Authorization: Bearer ${env}\"")
        });
        format!(
            "export PATH=\"$HOME/.local/bin:$HOME/.claude/local:$PATH\"\n\
             claude mcp add '{}' '{}' --transport http --scope user{header}",
            self.name, self.url
        )
    }
}

impl Mixin {
    /// A mixin that registers `servers` in the configuration of the agent
    /// `agent`, at install time, and allows their hosts. The tokens are given
    /// at creation, in the variables of [`McpServer::token_env`].
    ///
    /// ```no_run
    /// # async fn run(client: docker_sandbox::Client) -> docker_sandbox_kit::Result<()> {
    /// use docker_sandbox_kit::{Agent, KitForge, McpServer, Mixin};
    ///
    /// let tools = McpServer::host("tools", 47813);
    /// let package = KitForge::agent(client.clone(), Agent::Claude)
    ///     .with_mixin(Mixin::mcp(Agent::Claude, [tools.clone()])?)
    ///     .build()
    ///     .await?;
    /// let token = "a random token of this sandbox";
    /// let request = package
    ///     .request("job-42")?
    ///     .with_env(tools.token_env().unwrap_or_default(), token);
    /// package.create_with(&client, &request).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidKit`] for an agent without an MCP adapter (all
    /// but claude for now), no server, two servers of the same name, a name
    /// or a variable that is not one, or a URL that is not an http or https
    /// one or holds a character a shell command cannot carry.
    pub fn mcp(agent: Agent, servers: impl IntoIterator<Item = McpServer>) -> Result<Self> {
        Self::mcp_checked(agent, &servers.into_iter().collect::<Vec<_>>())
    }

    /// [`Mixin::mcp`], compiled once whatever the type of its servers.
    fn mcp_checked(agent: Agent, servers: &[McpServer]) -> Result<Self> {
        let kit = format!("docker-sandbox-mcp-{agent}");
        let invalid = |message: String| Error::InvalidKit {
            kit: kit.clone(),
            message,
        };
        if agent != Agent::Claude {
            return Err(invalid(format!(
                "{agent} has no MCP adapter yet: only claude has one"
            )));
        }
        if servers.is_empty() {
            return Err(invalid("no MCP server to register".to_owned()));
        }
        let mut descriptor = KitDescriptor::mixin(format!("MCP servers for {agent}"))
            .with_description(format!(
                "Registers {} MCP server(s) in the configuration of {agent}",
                servers.len()
            ));
        for (index, server) in servers.iter().enumerate() {
            server.check().map_err(invalid)?;
            if servers[..index].iter().any(|s| s.name == server.name) {
                return Err(invalid(format!(
                    "two MCP servers are named {}",
                    server.name
                )));
            }
            let step = LifecycleStep::new(server.claude_command())
                .as_user("agent")
                .with_description(format!("Registers the MCP server {}", server.name))
                .with_env(server.token_env());
            descriptor = descriptor
                .with_install(step)
                .with_network_allow([server.authority()]);
        }
        Self::new(format!("/kits/{kit}"), descriptor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_remote_servers_name_their_url_and_their_token() {
        let tools = McpServer::host("project-tools", 47813);
        assert_eq!(tools.name(), "project-tools");
        assert_eq!(tools.url(), "http://host.docker.internal:47813/mcp");
        assert_eq!(
            tools.token_env(),
            Some("DOCKER_SANDBOX_MCP_PROJECT_TOOLS_TOKEN")
        );
        assert_eq!(tools.authority(), "host.docker.internal:47813");

        let docs = McpServer::remote("docs", "https://mcp.example.com/v1?x#y");
        assert_eq!(docs.token_env(), None);
        assert_eq!(docs.authority(), "mcp.example.com");
        assert_eq!(
            docs.with_bearer_env("DOCS_TOKEN").token_env(),
            Some("DOCS_TOKEN")
        );
    }

    #[test]
    fn the_claude_mixin_registers_each_server_at_install_time() {
        let tools = McpServer::host("tools", 47813);
        let docs = McpServer::remote("docs", "https://mcp.example.com/mcp");
        let mixin = Mixin::mcp(Agent::Claude, [tools, docs]).unwrap();
        assert_eq!(mixin.name(), "docker-sandbox-mcp-claude");
        let spec = mixin.descriptor().to_json();
        let steps = &spec["capabilities"][0]["config"]["install"];
        assert_eq!(
            steps[0]["command"],
            "export PATH=\"$HOME/.local/bin:$HOME/.claude/local:$PATH\"\n\
             claude mcp add 'tools' 'http://host.docker.internal:47813/mcp' --transport http \
             --scope user --header \"Authorization: Bearer $DOCKER_SANDBOX_MCP_TOOLS_TOKEN\""
        );
        assert_eq!(steps[0]["user"], "agent");
        assert_eq!(
            steps[0]["env"],
            serde_json::json!(["DOCKER_SANDBOX_MCP_TOOLS_TOKEN"])
        );
        assert!(
            steps[1]["command"]
                .as_str()
                .unwrap()
                .ends_with("'docs' 'https://mcp.example.com/mcp' --transport http --scope user")
        );
        assert!(steps[1].get("env").is_none(), "no token, no variable");
        assert_eq!(
            spec["capabilities"][1]["config"]["runtime"]["allow"],
            serde_json::json!(["host.docker.internal:47813", "mcp.example.com"])
        );
    }

    #[test]
    fn what_the_mixin_cannot_carry_is_refused() {
        let refused = |agent: Agent, servers: Vec<McpServer>| {
            Mixin::mcp(agent, servers).unwrap_err().to_string()
        };
        let tools = || McpServer::host("tools", 1);
        for (agent, servers, expected) in [
            (Agent::Codex, vec![tools()], "codex has no MCP adapter yet"),
            (Agent::Claude, vec![], "no MCP server to register"),
            (
                Agent::Claude,
                vec![McpServer::host("mcp-gateway", 1)],
                "the name the kit registers the gateway",
            ),
            (
                Agent::Claude,
                vec![tools(), tools()],
                "two MCP servers are named tools",
            ),
            (
                Agent::Claude,
                vec![McpServer::host("", 1)],
                "is not a name of MCP server",
            ),
            (
                Agent::Claude,
                vec![McpServer::host("a b", 1)],
                "is not a name of MCP server",
            ),
            (
                Agent::Claude,
                vec![McpServer::remote("d", "ftp://x")],
                "not an http or https one",
            ),
            (
                Agent::Claude,
                vec![McpServer::remote("d", "https://x/'a'")],
                "cannot carry",
            ),
            (
                Agent::Claude,
                vec![McpServer::remote("d", "https:///mcp")],
                "names no host",
            ),
            (
                Agent::Claude,
                vec![McpServer::remote("d", "https://x").with_bearer_env("1A")],
                "is not the name of an environment variable",
            ),
        ] {
            let error = refused(agent, servers);
            assert!(error.contains(expected), "{error}");
        }
    }
}
