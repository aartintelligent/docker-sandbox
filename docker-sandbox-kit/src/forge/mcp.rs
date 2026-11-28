//! MCP servers given to the agent of a sandbox by a mixin, with no file on
//! the host.
//!
//! The daemon only loads into the gateway of a sandbox the servers registered
//! with `sbx mcp add`, which writes a file of the state of the daemon. A mixin
//! registers a server in the configuration of the agent instead, inside the
//! sandbox and at install time, beside the gateway the kit of the agent
//! registers there at each start: `claude mcp add` for claude, and a merge
//! with `jq` into `~/.copilot/mcp-config.json` for copilot and into
//! `~/.config/opencode/opencode.json` for opencode, as their kits merge their
//! gateway. The server is served by a process of the host, on its loopback
//! ([`McpServer::host`]), remote ([`McpServer::remote`]), or started by the
//! agent inside the sandbox ([`McpServer::stdio`]).
//!
//! The bearer token of a server is a reference to a variable of the sandbox,
//! which the agent reads when it connects (`${VAR}` for claude and copilot,
//! `{env:VAR}` for opencode): the token is written in no file, and one forged
//! package serves sandboxes with tokens of their own, given at creation.
//!
//! A server is plain data, serialized as JSON by its kind, so that a program
//! in any language declares it.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

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

/// The first line of the commands that run Claude Code: where its kit
/// installs it.
const CLAUDE_PATH: &str = "export PATH=\"$HOME/.local/bin:$HOME/.claude/local:$PATH\"";

/// An MCP server for the agent of a sandbox, registered by [`Mixin::mcp`].
///
/// ```
/// use docker_sandbox_kit::McpServer;
///
/// let tools = McpServer::host("project-tools", 47813);
/// assert_eq!(tools.url(), Some("http://host.docker.internal:47813/mcp"));
/// assert_eq!(tools.token_env(), Some("DOCKER_SANDBOX_MCP_PROJECT_TOOLS_TOKEN"));
///
/// let linear = McpServer::remote("linear", "https://mcp.linear.app/mcp").with_bearer_env("LINEAR_TOKEN");
/// assert_eq!(linear.token_env(), Some("LINEAR_TOKEN"));
///
/// let files = McpServer::stdio("files", "mcp-server-filesystem").with_args(["/home/agent/workspace"]);
/// assert_eq!(files.url(), None);
/// ```
///
/// Its JSON form names its kind, and only what a program declares: the URL
/// and the variable of a server of the host follow from its name and port.
///
/// ```
/// use docker_sandbox_kit::McpServer;
///
/// let tools: McpServer = serde_json::from_str(r#"{"kind": "host", "name": "tools", "port": 47813}"#)?;
/// assert_eq!(tools, McpServer::host("tools", 47813));
/// assert_eq!(
///     serde_json::to_value(McpServer::stdio("files", "mcp-fs").with_args(["/work"]))?,
///     serde_json::json!({"kind": "stdio", "name": "files", "command": ["mcp-fs", "/work"]}),
/// );
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// Name the agent knows the server under.
    name: String,
    /// How the agent reaches the server.
    transport: Transport,
    /// Variable of the sandbox holding its bearer token, if it takes one.
    token_env: Option<String>,
    /// Arguments of the program of a stdio server.
    args: Vec<String>,
}

/// How the agent reaches an MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Transport {
    /// Streamable HTTP, to a process of the host.
    Host {
        /// Port of the loopback of the host the server listens on.
        port: u16,
        /// URL the sandbox reaches it at.
        url: String,
    },
    /// Streamable HTTP, to a remote server.
    Remote {
        /// URL of its endpoint.
        url: String,
    },
    /// Standard input and output of a program the agent starts in the
    /// sandbox.
    Stdio {
        /// The program, a path or a name the `PATH` of the sandbox finds.
        program: String,
    },
}

/// The JSON form of an [`McpServer`]: by kind, with only what a program
/// declares.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum Wire {
    /// A server of the host, [`McpServer::host`].
    Host {
        /// Name of the server.
        name: String,
        /// Port of the loopback of the host.
        port: u16,
        /// Variable of its token, when it is not the one its name gives.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_env: Option<String>,
    },
    /// A remote server, [`McpServer::remote`].
    Remote {
        /// Name of the server.
        name: String,
        /// URL of its endpoint.
        url: String,
        /// Variable of its token, if it takes one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_env: Option<String>,
    },
    /// A server started in the sandbox, [`McpServer::stdio`].
    Stdio {
        /// Name of the server.
        name: String,
        /// Its program, then its arguments.
        command: Vec<String>,
    },
}

impl McpServer {
    /// A server served by a process of the host, on `port` of its loopback,
    /// in any language: the sandbox reaches it at
    /// `http://host.docker.internal:<port>/mcp`, over streamable HTTP, and
    /// the server receives `Host: localhost:<port>`. Its bearer token is
    /// read from the variable [`McpServer::token_env`]
    /// (`DOCKER_SANDBOX_MCP_<NAME>_TOKEN`), which the creation request sets
    /// and the server checks. The proxy rewrites no request to the host: the
    /// sandbox holds that token, which is to be random and its own.
    pub fn host(name: impl Into<String>, port: u16) -> Self {
        let name = name.into();
        Self {
            token_env: Some(host_token_env(&name)),
            transport: Transport::Host {
                port,
                url: format!("http://{HOST}:{port}{HOST_PATH}"),
            },
            name,
            args: Vec::new(),
        }
    }

    /// A remote server at `url`, called by the agent itself, without a token
    /// until [`McpServer::with_bearer_env`].
    pub fn remote(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: Transport::Remote { url: url.into() },
            token_env: None,
            args: Vec::new(),
        }
    }

    /// A server the agent starts inside the sandbox, as the user `agent`, and
    /// speaks to over its standard input and output: `program`, a path or a
    /// name the `PATH` of the sandbox finds, with the arguments of
    /// [`McpServer::with_args`]. It inherits the environment of the sandbox,
    /// whose variables give it its secrets. Installing the program is the
    /// work of another mixin, or of the image of the kit.
    pub fn stdio(name: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: Transport::Stdio {
                program: program.into(),
            },
            token_env: None,
            args: Vec::new(),
        }
    }

    /// Gives the program of a stdio server `args`, after those it has. A
    /// server reached at a URL takes none: [`Mixin::mcp`] refuses it.
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sends the content of the variable `env` of the sandbox as the bearer
    /// token of the server, read by the agent when it connects. For a key the
    /// sandbox must not hold, set `env` to the placeholder of a `ProxiedKey`
    /// for the host of the server: the proxy writes the key in its place. A
    /// stdio server takes none: [`Mixin::mcp`] refuses it.
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

    /// URL of the endpoint of the server, unless it is a stdio one.
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        match &self.transport {
            Transport::Host { url, .. } | Transport::Remote { url } => Some(url),
            Transport::Stdio { .. } => None,
        }
    }

    /// Variable of the sandbox holding the bearer token of the server, if it
    /// takes one.
    #[must_use]
    pub fn token_env(&self) -> Option<&str> {
        self.token_env.as_deref()
    }

    /// The host of the URL, with its port when it has one: what the network
    /// rule of the mixin allows. A stdio server reaches no host by itself.
    fn authority(&self) -> Option<&str> {
        let rest = self.url()?.split_once("://").map_or("", |(_, rest)| rest);
        rest.split(['/', '?', '#']).next()
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
        match &self.transport {
            Transport::Stdio { program } => {
                if program.is_empty() {
                    return Err(format!("the MCP server {name} names no program"));
                }
                if self.token_env.is_some() {
                    return Err(format!(
                        "the MCP server {name} runs in the sandbox: it takes no bearer token, \
                         but the variables of the sandbox"
                    ));
                }
                Ok(())
            }
            Transport::Host { url, .. } | Transport::Remote { url } => {
                if !self.args.is_empty() {
                    return Err(format!(
                        "the MCP server {name} is reached at a URL: it takes no arguments"
                    ));
                }
                Self::check_url(name, url)?;
                match self.token_env() {
                    Some(env) if !is_variable_name(env) => Err(format!(
                        "{env:?} is not the name of an environment variable"
                    )),
                    _ => Ok(()),
                }
            }
        }
    }

    /// What is wrong with the URL of the server `name`, if anything.
    fn check_url(name: &str, url: &str) -> std::result::Result<(), String> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!("the URL of {name} is not an http or https one"));
        }
        // An agent reads `${…}` in a URL as a variable, and a shell some of
        // the others.
        if url
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '\\' | '`' | '$'))
        {
            return Err(format!(
                "the URL of {name} holds a character its command cannot carry"
            ));
        }
        let host = url.split_once("://").map_or("", |(_, rest)| rest);
        if host
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default()
            .is_empty()
        {
            return Err(format!("the URL of {name} names no host"));
        }
        Ok(())
    }

    /// The JSON form of the server, refused for a server [`McpServer::check`]
    /// would refuse for its kind: arguments for a URL, a token for a stdio
    /// server.
    fn wire(&self) -> std::result::Result<Wire, String> {
        let name = self.name.clone();
        match &self.transport {
            Transport::Stdio { .. } if self.token_env.is_some() => Err(format!(
                "the MCP server {name} runs in the sandbox: it takes no bearer token"
            )),
            Transport::Host { .. } | Transport::Remote { .. } if !self.args.is_empty() => Err(
                format!("the MCP server {name} is reached at a URL: it takes no arguments"),
            ),
            Transport::Host { port, .. } => Ok(Wire::Host {
                token_env: self
                    .token_env
                    .clone()
                    .filter(|env| *env != host_token_env(&name)),
                name,
                port: *port,
            }),
            Transport::Remote { url } => Ok(Wire::Remote {
                name,
                url: url.clone(),
                token_env: self.token_env.clone(),
            }),
            Transport::Stdio { program } => Ok(Wire::Stdio {
                name,
                command: std::iter::once(program.clone())
                    .chain(self.args.iter().cloned())
                    .collect(),
            }),
        }
    }

    /// The command that registers the server in the configuration of
    /// `agent`, one of those with an adapter.
    fn register(&self, agent: Agent) -> String {
        match agent {
            Agent::Claude => self.claude_command(),
            Agent::Copilot => self.copilot_command(),
            _ => self.opencode_command(),
        }
    }

    /// The command that registers the server in Claude Code: the form the
    /// claude kit registers its gateway in, or `--transport stdio`.
    fn claude_command(&self) -> String {
        let name = quote(&self.name);
        let registration = match &self.transport {
            Transport::Stdio { program } => {
                let command = std::iter::once(program)
                    .chain(&self.args)
                    .map(|part| quote(part))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("claude mcp add --scope user --transport stdio {name} -- {command}")
            }
            Transport::Host { url, .. } | Transport::Remote { url } => {
                let header = self.token_env().map_or_else(String::new, |env| {
                    format!(
                        " --header {}",
                        quote(&format!("Authorization: Bearer ${{{env}}}"))
                    )
                });
                format!(
                    "claude mcp add {name} {} --transport http --scope user{header}",
                    quote(url)
                )
            }
        };
        format!("{CLAUDE_PATH}\n{registration}")
    }

    /// The command that registers the server in `~/.copilot/mcp-config.json`,
    /// under the lock the copilot kit merges its gateway under.
    fn copilot_command(&self) -> String {
        let entry = match &self.transport {
            Transport::Stdio { program } => {
                json!({"type": "local", "command": program, "args": self.args, "tools": ["*"]})
            }
            Transport::Host { url, .. } | Transport::Remote { url } => {
                let mut entry = json!({"type": "http", "url": url, "tools": ["*"]});
                if let Some(env) = self.token_env() {
                    entry["headers"] = json!({"Authorization": format!("Bearer ${{{env}}}")});
                }
                entry
            }
        };
        merge(
            "/home/agent/.copilot",
            "mcp-config.json",
            Some(".mcp-config.lock"),
            "mcpServers",
            &json!({ self.name.as_str(): entry }),
        )
    }

    /// The command that registers the server in
    /// `~/.config/opencode/opencode.json`.
    fn opencode_command(&self) -> String {
        let entry = match &self.transport {
            Transport::Stdio { program } => {
                let command: Vec<&String> = std::iter::once(program).chain(&self.args).collect();
                json!({"type": "local", "command": command, "enabled": true})
            }
            Transport::Host { url, .. } | Transport::Remote { url } => {
                let mut entry = json!({"type": "remote", "url": url, "enabled": true});
                if let Some(env) = self.token_env() {
                    entry["headers"] = json!({"Authorization": format!("Bearer {{env:{env}}}")});
                }
                entry
            }
        };
        merge(
            "/home/agent/.config/opencode",
            "opencode.json",
            None,
            "mcp",
            &json!({ self.name.as_str(): entry }),
        )
    }
}

impl Serialize for McpServer {
    /// Writes the JSON form of the server.
    ///
    /// # Errors
    ///
    /// Fails for a server reached at a URL given arguments, or a stdio server
    /// given a bearer token, which no JSON form describes.
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.wire()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for McpServer {
    /// Reads the JSON form of a server, checked by [`Mixin::mcp`] as the
    /// servers it is given.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Host {
                name,
                port,
                token_env,
            } => {
                let server = Self::host(name, port);
                match token_env {
                    Some(env) => server.with_bearer_env(env),
                    None => server,
                }
            }
            Wire::Remote {
                name,
                url,
                token_env,
            } => Self {
                token_env,
                ..Self::remote(name, url)
            },
            Wire::Stdio { name, command } => {
                let mut command = command.into_iter();
                Self::stdio(name, command.next().unwrap_or_default()).with_args(command)
            }
        })
    }
}

impl Mixin {
    /// A mixin that registers `servers` in the configuration of the agent
    /// `agent`, at install time and as the user `agent`, and allows the
    /// hosts of those reached at a URL. Claude, copilot and opencode have an
    /// adapter. The tokens are given at creation, in the variables of
    /// [`McpServer::token_env`], which the agent reads when it connects.
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
    /// Returns [`Error::InvalidKit`] for an agent without an MCP adapter, no
    /// server, two servers of the same name, a name or a variable that is not
    /// one, a URL that is not an http or https one or holds a character a
    /// shell command cannot carry, a stdio server without a program or given
    /// a bearer token, or a server reached at a URL given arguments.
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
        if !matches!(agent, Agent::Claude | Agent::Copilot | Agent::OpenCode) {
            return Err(invalid(format!(
                "{agent} has no MCP adapter yet: only claude, copilot and opencode have one"
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
        let mut hosts = Vec::new();
        for (index, server) in servers.iter().enumerate() {
            server.check().map_err(invalid)?;
            if servers[..index].iter().any(|s| s.name == server.name) {
                return Err(invalid(format!(
                    "two MCP servers are named {}",
                    server.name
                )));
            }
            let step = LifecycleStep::new(server.register(agent))
                .as_user("agent")
                .with_description(format!("Registers the MCP server {}", server.name));
            descriptor = descriptor.with_install(step);
            hosts.extend(server.authority());
        }
        if !hosts.is_empty() {
            descriptor = descriptor.with_network_allow(hosts);
        }
        Self::new(format!("/kits/{kit}"), descriptor)
    }
}

/// The variable a server of the host named `name` reads its token from.
fn host_token_env(name: &str) -> String {
    format!(
        "DOCKER_SANDBOX_MCP_{}_TOKEN",
        name.to_ascii_uppercase().replace('-', "_")
    )
}

/// `text` quoted for a POSIX shell, whatever it holds.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The command that merges `servers` into the object `key` of the JSON file
/// `file` of `dir`, as the user `agent`, with `jq`, under the lock `lock` of
/// `dir` when the kit merges its gateway under one: the way the kits of
/// copilot and opencode register their gateway, which keeps the servers
/// already there.
fn merge(dir: &str, file: &str, lock: Option<&str>, key: &str, servers: &Value) -> String {
    let lock = lock.map_or_else(String::new, |lock| {
        format!("exec 9>>\"$dir/{lock}\"\nflock 9\n")
    });
    format!(
        "set -e\n\
         dir={dir}\n\
         cfg=\"$dir/{file}\"\n\
         mkdir -p \"$dir\"\n\
         {lock}\
         [ -s \"$cfg\" ] || echo '{{}}' > \"$cfg\"\n\
         tmp=$(mktemp \"$cfg.XXXXXX\")\n\
         trap 'rm -f \"$tmp\"' EXIT\n\
         jq --argjson servers {} '.{key} = ((.{key} // {{}}) + $servers)' \"$cfg\" > \"$tmp\"\n\
         mv -f \"$tmp\" \"$cfg\"",
        quote(&servers.to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The install steps of a mixin, as JSON.
    fn steps(mixin: &Mixin) -> Vec<Value> {
        let spec = mixin.descriptor().to_json();
        spec["capabilities"][0]["config"]["install"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// The command of the install step `index` of a mixin.
    fn command(mixin: &Mixin, index: usize) -> String {
        steps(mixin)[index]["command"].as_str().unwrap().to_owned()
    }

    /// The servers a merge command writes, read back from its `--argjson`.
    fn merged(command: &str) -> Value {
        let quoted = command
            .split("--argjson servers ")
            .nth(1)
            .unwrap()
            .split(" '.")
            .next()
            .unwrap();
        let json = quoted
            .strip_prefix('\'')
            .unwrap()
            .strip_suffix('\'')
            .unwrap()
            .replace(r"'\''", "'");
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn servers_name_their_url_and_their_token() {
        let tools = McpServer::host("project-tools", 47813);
        assert_eq!(tools.name(), "project-tools");
        assert_eq!(tools.url(), Some("http://host.docker.internal:47813/mcp"));
        assert_eq!(
            tools.token_env(),
            Some("DOCKER_SANDBOX_MCP_PROJECT_TOOLS_TOKEN")
        );
        assert_eq!(tools.authority(), Some("host.docker.internal:47813"));

        let docs = McpServer::remote("docs", "https://mcp.example.com/v1?x#y");
        assert_eq!(docs.token_env(), None);
        assert_eq!(docs.authority(), Some("mcp.example.com"));
        assert_eq!(
            docs.with_bearer_env("DOCS_TOKEN").token_env(),
            Some("DOCS_TOKEN")
        );

        let files = McpServer::stdio("files", "mcp-fs").with_args(["/work"]);
        assert_eq!(
            (files.url(), files.token_env(), files.authority()),
            (None, None, None)
        );
    }

    #[test]
    fn the_claude_mixin_registers_each_server_at_install_time() {
        let tools = McpServer::host("tools", 47813);
        let docs = McpServer::remote("docs", "https://mcp.example.com/mcp");
        let files = McpServer::stdio("files", "/opt/mcp fs").with_args(["it's", "--x"]);
        let mixin = Mixin::mcp(Agent::Claude, [tools, docs, files]).unwrap();
        assert_eq!(mixin.name(), "docker-sandbox-mcp-claude");
        assert_eq!(
            command(&mixin, 0),
            "export PATH=\"$HOME/.local/bin:$HOME/.claude/local:$PATH\"\n\
             claude mcp add 'tools' 'http://host.docker.internal:47813/mcp' --transport http \
             --scope user --header 'Authorization: Bearer ${DOCKER_SANDBOX_MCP_TOOLS_TOKEN}'"
        );
        let step = &steps(&mixin)[0];
        assert_eq!(step["user"], "agent");
        assert!(
            step.get("env").is_none(),
            "the agent reads the token itself"
        );
        assert!(
            command(&mixin, 1)
                .ends_with("'docs' 'https://mcp.example.com/mcp' --transport http --scope user")
        );
        assert!(command(&mixin, 2).ends_with(
            "claude mcp add --scope user --transport stdio 'files' -- '/opt/mcp fs' 'it'\\''s' '--x'"
        ));
        let spec = mixin.descriptor().to_json();
        assert_eq!(
            spec["capabilities"][1]["config"]["runtime"]["allow"],
            json!(["host.docker.internal:47813", "mcp.example.com"])
        );
    }

    #[test]
    fn the_copilot_mixin_merges_each_server_under_the_lock_of_its_kit() {
        let tools = McpServer::host("tools", 47813);
        let files = McpServer::stdio("files", "mcp-fs").with_args(["it's"]);
        let docs = McpServer::remote("docs", "https://mcp.example.com/mcp");
        let mixin = Mixin::mcp(Agent::Copilot, [tools, files, docs]).unwrap();
        assert_eq!(mixin.name(), "docker-sandbox-mcp-copilot");
        let first = command(&mixin, 0);
        assert!(first.starts_with(
            "set -e\ndir=/home/agent/.copilot\ncfg=\"$dir/mcp-config.json\"\nmkdir -p \"$dir\"\n\
             exec 9>>\"$dir/.mcp-config.lock\"\nflock 9\n"
        ));
        assert!(first.ends_with(
            "'.mcpServers = ((.mcpServers // {}) + $servers)' \"$cfg\" > \"$tmp\"\n\
             mv -f \"$tmp\" \"$cfg\""
        ));
        assert_eq!(
            merged(&first),
            json!({"tools": {
                "type": "http",
                "url": "http://host.docker.internal:47813/mcp",
                "tools": ["*"],
                "headers": {"Authorization": "Bearer ${DOCKER_SANDBOX_MCP_TOOLS_TOKEN}"},
            }})
        );
        assert_eq!(
            merged(&command(&mixin, 1)),
            json!({"files": {"type": "local", "command": "mcp-fs", "args": ["it's"], "tools": ["*"]}})
        );
        assert_eq!(
            merged(&command(&mixin, 2)),
            json!({"docs": {"type": "http", "url": "https://mcp.example.com/mcp", "tools": ["*"]}})
        );
    }

    #[test]
    fn the_opencode_mixin_merges_each_server_into_its_configuration() {
        let docs = McpServer::remote("docs", "https://mcp.example.com/mcp");
        let tools = McpServer::host("tools", 47813);
        let files = McpServer::stdio("files", "mcp-fs").with_args(["/work"]);
        let mixin = Mixin::mcp(Agent::OpenCode, [docs, tools, files]).unwrap();
        let first = command(&mixin, 0);
        assert!(first.starts_with(
            "set -e\ndir=/home/agent/.config/opencode\ncfg=\"$dir/opencode.json\"\n\
             mkdir -p \"$dir\"\n[ -s \"$cfg\" ]"
        ));
        assert!(
            !first.contains("flock"),
            "the opencode kit merges under no lock"
        );
        assert!(first.contains("'.mcp = ((.mcp // {}) + $servers)'"));
        assert_eq!(
            merged(&first),
            json!({"docs": {"type": "remote", "url": "https://mcp.example.com/mcp", "enabled": true}})
        );
        assert_eq!(
            merged(&command(&mixin, 1))["tools"]["headers"],
            json!({"Authorization": "Bearer {env:DOCKER_SANDBOX_MCP_TOOLS_TOKEN}"})
        );
        assert_eq!(
            merged(&command(&mixin, 2)),
            json!({"files": {"type": "local", "command": ["mcp-fs", "/work"], "enabled": true}})
        );
    }

    #[test]
    fn a_mixin_of_stdio_servers_alone_allows_no_host() {
        let mixin = Mixin::mcp(Agent::Claude, [McpServer::stdio("files", "mcp-fs")]).unwrap();
        let spec = mixin.descriptor().to_json();
        assert_eq!(spec["capabilities"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn servers_are_written_and_read_as_json_by_their_kind() {
        for (server, form) in [
            (
                McpServer::host("tools", 47813),
                json!({"kind": "host", "name": "tools", "port": 47813}),
            ),
            (
                McpServer::host("tools", 1).with_bearer_env("MY_TOKEN"),
                json!({"kind": "host", "name": "tools", "port": 1, "token_env": "MY_TOKEN"}),
            ),
            (
                McpServer::remote("docs", "https://x/mcp"),
                json!({"kind": "remote", "name": "docs", "url": "https://x/mcp"}),
            ),
            (
                McpServer::remote("docs", "https://x/mcp").with_bearer_env("DOCS"),
                json!({"kind": "remote", "name": "docs", "url": "https://x/mcp", "token_env": "DOCS"}),
            ),
            (
                McpServer::stdio("files", "mcp-fs").with_args(["/a", "/b"]),
                json!({"kind": "stdio", "name": "files", "command": ["mcp-fs", "/a", "/b"]}),
            ),
        ] {
            assert_eq!(serde_json::to_value(&server).unwrap(), form);
            assert_eq!(serde_json::from_value::<McpServer>(form).unwrap(), server);
        }
        let empty: McpServer =
            serde_json::from_value(json!({"kind": "stdio", "name": "e", "command": []})).unwrap();
        assert_eq!(empty, McpServer::stdio("e", ""));
        for refused in [
            json!({"kind": "host", "name": "t", "port": 1, "url": "http://x"}),
            json!({"kind": "socket", "name": "t"}),
            json!({"name": "t", "port": 1}),
        ] {
            assert!(serde_json::from_value::<McpServer>(refused).is_err());
        }
    }

    #[test]
    fn a_server_no_json_form_describes_is_not_written() {
        for (server, expected) in [
            (
                McpServer::stdio("files", "mcp-fs").with_bearer_env("TOKEN"),
                "files runs in the sandbox: it takes no bearer token",
            ),
            (
                McpServer::remote("docs", "https://x").with_args(["a"]),
                "docs is reached at a URL: it takes no arguments",
            ),
            (
                McpServer::host("tools", 1).with_args(["a"]),
                "tools is reached at a URL: it takes no arguments",
            ),
        ] {
            let error = serde_json::to_value(&server).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
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
                Agent::Copilot,
                vec![McpServer::host("a b", 1)],
                "is not a name of MCP server",
            ),
            (
                Agent::Claude,
                vec![McpServer::remote("d", "ftp://x")],
                "not an http or https one",
            ),
            (
                Agent::OpenCode,
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
            (
                Agent::Claude,
                vec![McpServer::remote("d", "https://x").with_args(["a"])],
                "is reached at a URL: it takes no arguments",
            ),
            (
                Agent::Copilot,
                vec![McpServer::stdio("s", "")],
                "the MCP server s names no program",
            ),
            (
                Agent::OpenCode,
                vec![McpServer::stdio("s", "p").with_bearer_env("TOKEN")],
                "runs in the sandbox: it takes no bearer token",
            ),
        ] {
            let error = refused(agent, servers);
            assert!(error.contains(expected), "{error}");
        }
    }
}
