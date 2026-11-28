//! The rules a spec meets before it is launched, and the kit it forges.
//!
//! A spec is plain data until it is checked: [`SandboxSpec::check`] applies
//! the rules a launch needs, then the checks of the forge, the kit policy of
//! the daemon included, without pulling or writing anything. Each rule keeps
//! a sandbox from being created that could not do what its spec says. This
//! module holds the rules that span the domains (one base kit, absolute
//! mounts, variables set once) and calls those of each domain: the network
//! rules, the targets of the keys, the tokens of the MCP servers. The
//! arguments of the base kit are checked against its descriptor only when it
//! is forged, as the descriptor comes with its image.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::Path;

use docker_sandbox::Client;
use docker_sandbox::kit::KitPackage;
use docker_sandbox_kit::{Agent, KitForge, Mixin};

use super::{SandboxSpec, WorkspaceSpec};
use crate::error::{Result, invalid};

/// The kit of a spec, ready to forge.
#[derive(Debug)]
pub(crate) struct Prepared<'a> {
    /// Its base kit.
    base: BaseKit<'a>,
    /// Its mixins: those of the program, then those it generates.
    pub(crate) mixins: Vec<Mixin>,
}

/// The base kit of a spec.
#[derive(Debug)]
enum BaseKit<'a> {
    /// The kit of an agent.
    Agent(Agent),
    /// A published kit, by its reference.
    Published(&'a str),
    /// A compiled package, read from its directory.
    Package(KitPackage),
}

/// The kit of a spec: a package launched as it is, or a forge.
#[derive(Debug)]
pub(crate) enum Kit {
    /// A compiled package and no mixin: the package itself.
    Package(KitPackage),
    /// A composition to forge.
    Forge(KitForge),
}

/// The variables a spec sets, with what sets each.
pub(crate) type Owners<'a> = BTreeMap<&'a str, String>;

impl SandboxSpec {
    /// Checks the spec against the rules of a launch, then its kit as
    /// [`KitForge::check`] does, the kit policy of the daemon included.
    /// Nothing is pulled or written; the mixins of a directory are read.
    ///
    /// The rules:
    ///
    /// - exactly one of `agent`, `base` and `package`;
    /// - a package that suits the daemon, given no `args`, and holding no
    ///   mixin of its own when the spec adds some, as the forge takes its
    ///   base kit alone;
    /// - MCP servers for an agent `Mixin::mcp` takes them for: claude,
    ///   copilot or opencode, named by `agent` or declared by the package;
    /// - every host of `network` a host pattern, which the daemon would
    ///   otherwise store and never apply;
    /// - no refusal of `network` covering a host a key goes to or an MCP
    ///   server is reached at, as a refusal wins over any allowance;
    /// - the variables of `keys`, of `env` and of the tokens of the servers
    ///   of the host names of variables, none set twice;
    /// - the token of a remote server set by a key or by `env`;
    /// - every workspace and mount an absolute path;
    /// - every mixin one the forge takes.
    ///
    /// ```no_run
    /// # async fn run(client: docker_sandbox::Client) -> docker_sandbox_blueprint::Result<()> {
    /// use docker_sandbox_blueprint::SandboxSpec;
    /// use docker_sandbox_kit::Agent;
    ///
    /// let spec = SandboxSpec::agent(Agent::Claude).with_network_deny(["*.pastebin.com"]);
    /// spec.check(&client).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSpec`](crate::Error::InvalidSpec) for a rule
    /// the spec breaks, [`Error::Kit`](crate::Error::Kit) when a mixin or an
    /// MCP server is not one the forge takes or the forge refuses the kit,
    /// and [`Error::Client`](crate::Error::Client) when the daemon cannot be
    /// read.
    pub async fn check(&self, client: &Client) -> Result<()> {
        if let Kit::Forge(forge) = self.kit(client)? {
            forge.check().await?;
        }
        Ok(())
    }

    /// The kit of the spec, once the spec meets the rules of a launch: a
    /// compiled package without mixins as it is, or the forge of its base
    /// kit and its arguments, the mixins of the program, then those of the
    /// network rules and of the MCP servers.
    pub(crate) fn kit(&self, client: &Client) -> Result<Kit> {
        let prepared = self.prepare()?;
        let mut forge = match prepared.base {
            BaseKit::Agent(agent) => KitForge::agent(client.clone(), agent),
            BaseKit::Published(reference) => KitForge::new(client.clone()).with_base(reference),
            BaseKit::Package(package) => {
                if !package.suits(client.health()) {
                    return Err(invalid(format!(
                        "package: the package was made for another daemon ({}), not sbx {} of \
                         API {}: make it again",
                        package
                            .compiled_with()
                            .or(package.compiled_api())
                            .unwrap_or("unrecorded"),
                        client.health().version,
                        client.health().api_version
                    )));
                }
                if prepared.mixins.is_empty() {
                    return Ok(Kit::Package(package));
                }
                // The forge takes the base kit of a package alone: the mixins
                // compiled into it would be dropped without a word.
                let kits = package.kit_refs();
                if kits.len() > 1 {
                    return Err(invalid(format!(
                        "package: the package composes mixins of its own ({}), and the spec \
                         adds others: launch it as it is, or give the package of its base kit \
                         alone",
                        kits[1..].join(", ")
                    )));
                }
                KitForge::new(client.clone()).with_base_package(package)
            }
        };
        for (name, value) in &self.args {
            forge = forge.with_arg(name, value);
        }
        for mixin in prepared.mixins {
            forge = forge.with_mixin(mixin);
        }
        Ok(Kit::Forge(forge))
    }

    /// Checks the rules of a launch that need no daemon, and reads or
    /// writes the mixins of the kit.
    pub(crate) fn prepare(&self) -> Result<Prepared<'_>> {
        let base = self.base_kit()?;
        self.check_paths()?;
        self.check_key_targets()?;
        self.check_variables()?;
        self.check_reach()?;
        let mut mixins = Vec::new();
        for mixin in &self.mixins {
            mixins.push(mixin.read()?);
        }
        let agent = match &base {
            BaseKit::Agent(agent) => Some(*agent),
            BaseKit::Published(_) => None,
            BaseKit::Package(package) => package
                .agent()
                .and_then(|agent| agent.name.parse::<Agent>().ok()),
        };
        for generated in [self.network_mixin(), self.mcp_mixin(agent)] {
            mixins.extend(generated?);
        }
        Ok(Prepared { base, mixins })
    }

    /// The base kit of the spec, named once, its package read.
    fn base_kit(&self) -> Result<BaseKit<'_>> {
        let named = [
            self.agent.is_some(),
            self.base.is_some(),
            self.package.is_some(),
        ];
        match named.iter().filter(|named| **named).count() {
            0 => {
                return Err(invalid(
                    "no base kit: name an agent, a published kit or a package",
                ));
            }
            1 => {}
            _ => {
                return Err(invalid(
                    "agent, base and package: a spec names its base kit once, by its agent, its \
                     reference or its package",
                ));
            }
        }
        if let Some(agent) = self.agent {
            return Ok(BaseKit::Agent(agent));
        }
        if let Some(reference) = self.base.as_deref() {
            return Ok(BaseKit::Published(reference));
        }
        if !self.args.is_empty() {
            return Err(invalid(
                "args: a compiled package keeps the arguments it was compiled with",
            ));
        }
        let dir = self.package.clone().unwrap_or_default();
        Ok(BaseKit::Package(KitPackage::open(dir)?))
    }

    /// Checks that the variables of the keys, of `env` and of the tokens of
    /// the MCP servers are set once, and that the token of a remote server
    /// is set.
    fn check_variables(&self) -> Result<()> {
        let mut owners = Owners::new();
        for key in &self.keys {
            claim(
                &mut owners,
                key.env(),
                format!("keys: the key {}", key.env()),
            )?;
        }
        for name in self.env.keys() {
            claim(&mut owners, name, "env".to_owned())?;
        }
        for (server, env) in self.host_tokens() {
            let owner = format!("mcp: the token of the server {}", server.name());
            claim(&mut owners, env, owner)?;
        }
        self.check_remote_tokens(&owners)
    }

    /// Checks that the workspace and the mounts are absolute paths.
    fn check_paths(&self) -> Result<()> {
        let absolute = |field: &str, path: &str| {
            if Path::new(path).is_absolute() {
                Ok(())
            } else {
                Err(invalid(format!(
                    "{field}: {path:?} is not an absolute path, which a mount needs"
                )))
            }
        };
        match &self.workspace {
            Some(WorkspaceSpec::Mount(dir)) => absolute("workspace", dir)?,
            Some(WorkspaceSpec::Clone(repository)) => absolute("workspace", repository)?,
            None => {}
        }
        for mount in &self.mounts {
            absolute("mounts", &mount.dir)?;
            if let Some(target) = &mount.target {
                absolute("mounts", target)?;
            }
        }
        Ok(())
    }
}

/// Records that `owner` sets the variable `env`.
fn claim<'a>(owners: &mut Owners<'a>, env: &'a str, owner: String) -> Result<()> {
    if !is_variable_name(env) {
        return Err(invalid(format!(
            "{owner}: {env:?} is not the name of an environment variable"
        )));
    }
    match owners.entry(env) {
        Entry::Occupied(first) => Err(invalid(format!(
            "{env} is set by {} and by {owner}: one value reaches the sandbox",
            first.get()
        ))),
        Entry::Vacant(entry) => {
            entry.insert(owner);
            Ok(())
        }
    }
}

/// Whether `name` is the name of an environment variable.
fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The message of the rule `spec` breaks, for the tests of every domain.
#[cfg(test)]
pub(crate) fn broken(spec: &SandboxSpec) -> String {
    let error = spec.prepare().unwrap_err().to_string();
    error
        .strip_prefix("invalid sandbox spec: ")
        .unwrap()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use docker_sandbox::sandbox::Workspace;
    use docker_sandbox_kit::McpServer;

    use super::*;
    use crate::key::KeySpec;

    /// A claude spec.
    fn claude() -> SandboxSpec {
        SandboxSpec::agent(Agent::Claude)
    }

    #[test]
    fn a_spec_names_one_base_kit_and_composes_its_mixins() {
        let mut both = claude();
        both.base = Some("docker.io/sbx/claude:latest".to_owned());
        assert!(broken(&both).starts_with("agent, base and package"));
        let mut none = claude();
        none.agent = None;
        assert!(broken(&none).starts_with("no base kit"));
        let args = SandboxSpec::package("/var/lib/kits/shell").with_arg("model", "m");
        assert!(broken(&args).starts_with("args: a compiled package keeps"));
        let missing = SandboxSpec::package("/nonexistent/docker-sandbox-blueprint/package");
        let error = missing.prepare().unwrap_err().to_string();
        assert!(
            error.contains("/nonexistent/docker-sandbox-blueprint/package"),
            "{error}"
        );
        let missing = claude().with_mixin_dir("/nonexistent/docker-sandbox-blueprint/mixin");
        let error = missing.prepare().unwrap_err().to_string();
        assert!(
            error.contains("/nonexistent/docker-sandbox-blueprint/mixin"),
            "{error}"
        );
        let spec = claude()
            .with_network_deny(["*.pastebin.com"])
            .with_mcp(McpServer::host("tools", 47813));
        let prepared = spec.prepare().unwrap();
        let names: Vec<&str> = prepared.mixins.iter().map(Mixin::name).collect();
        assert_eq!(
            names,
            ["docker-sandbox-network", "docker-sandbox-mcp-claude"]
        );
    }

    #[test]
    fn variables_are_set_once() {
        let twice = claude()
            .with_key(KeySpec::for_service("TOKEN", "anthropic"))
            .with_env("TOKEN", "x");
        assert_eq!(
            broken(&twice),
            "TOKEN is set by keys: the key TOKEN and by env: one value reaches the sandbox"
        );
        let token = claude()
            .with_mcp(McpServer::host("tools", 47813))
            .with_env("DOCKER_SANDBOX_MCP_TOOLS_TOKEN", "x");
        assert!(broken(&token).contains("by mcp: the token of the server tools"));
        let keys = claude()
            .with_key(KeySpec::for_service("A", "anthropic"))
            .with_key(KeySpec::for_hosts("A", ["api.example.com"]));
        assert!(broken(&keys).starts_with("A is set by keys: the key A and by keys: the key A"));
        let key = claude().with_key(KeySpec::for_service("NOT A NAME", "anthropic"));
        assert!(broken(&key).starts_with("keys: the key NOT A NAME: "));
        let name = claude().with_env("NOT A NAME", "x");
        assert!(broken(&name).contains("is not the name of an environment variable"));
        assert!(!is_variable_name("") && !is_variable_name("1A") && is_variable_name("_A1"));
    }

    #[test]
    fn mounts_are_absolute_paths() {
        assert!(broken(&claude().with_workspace("project")).starts_with("workspace: "));
        assert!(broken(&claude().with_cloned_workspace("./repo")).starts_with("workspace: "));
        assert!(broken(&claude().with_mount(Workspace::new("docs"))).starts_with("mounts: "));
        let target = claude().with_mount(Workspace::new("/srv/docs").at("docs"));
        assert!(broken(&target).starts_with("mounts: "));
        claude()
            .with_cloned_workspace("/srv/repo")
            .with_mount(Workspace::new("/srv/docs").at("/docs"))
            .prepare()
            .unwrap();
    }
}
