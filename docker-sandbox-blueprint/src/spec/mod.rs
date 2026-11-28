//! A whole sandbox described as data: the spec, its kit, its workspace, its
//! variables and its resources.
//!
//! A [`SandboxSpec`] holds everything a sandbox receives but its name and the
//! values of its keys, given at launch: the same spec serves every sandbox
//! of a job. Its JSON form is what a program in another language writes, and
//! an unknown field is refused. It is plain data: built in Rust or read from
//! JSON, it is checked only by [`SandboxSpec::check`], which names the rule a
//! spec breaks.
//!
//! This module owns the spec and the fields of the sandbox itself; each
//! other domain owns its own fields, their methods and their rules: the
//! network rules in `network`, the keys in `key`, the MCP servers in `mcp`.

mod check;
mod mixin;

pub(crate) use self::check::Owners;
#[cfg(test)]
pub(crate) use self::check::broken;
pub(crate) use self::mixin::MixinSpec;

use std::collections::BTreeMap;

use docker_sandbox::sandbox::Workspace;
use docker_sandbox_kit::{Agent, McpServer};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::key::KeySpec;
use crate::network::Network;

/// A whole sandbox described as data: everything it receives but its name
/// and the values of its keys.
///
/// It is written with chained methods, from [`SandboxSpec::agent`] or
/// [`SandboxSpec::base`]:
///
/// ```
/// use docker_sandbox_blueprint::prelude::*;
///
/// let spec = SandboxSpec::agent(Agent::Claude)
///     .with_arg("model", "claude-haiku-5-5")
///     .with_workspace("/srv/project")
///     .with_network_deny(["*.pastebin.com"])
///     .with_key(KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic"))
///     .with_mcp(McpServer::host("tools", 47813));
/// # let _ = spec;
/// ```
///
/// Its JSON form, for a program in another language, has the same fields:
///
/// | Field | Form | Becomes |
/// |---|---|---|
/// | `agent` or `base` | an agent (`"claude"`) or a published kit reference | `KitForge::agent` or `KitForge::with_base` |
/// | `args` | name → value | `KitForge::with_arg` |
/// | `mixins` | `{"dir"}` or `{"reference", "descriptor"}` | `Mixin::read` or `Mixin::new` |
/// | `workspace` | `{"mount": dir}` or `{"clone": repository}` | `with_workspace` or `with_cloned_workspace` |
/// | `mounts` | `{"dir", "read_only"?, "at"?}` | `with_additional_workspace` |
/// | `network` | `{"allow"?, "deny"?}`, host patterns | a generated `network-policy@1` mixin |
/// | `keys` | [`KeySpec`] | `ProxiedKey` |
/// | `mcp` | [`McpServer`] | `Mixin::mcp` |
/// | `env` | name → value | `with_envs`, in clear in the request: never a secret |
/// | `resources` | `{"cpus"?, "memory"?}` | `with_cpus`, `with_memory` |
/// | `keep_on_failure` | boolean | whether a failed launch keeps the sandbox |
///
/// ```
/// use docker_sandbox_blueprint::prelude::*;
///
/// let spec: SandboxSpec = serde_json::from_value(serde_json::json!({
///     "agent": "claude",
///     "args": { "model": "claude-haiku-5-5" },
///     "workspace": { "mount": "/srv/project" },
///     "network": { "deny": ["*.pastebin.com"] },
///     "keys": [{ "env": "ANTHROPIC_API_KEY", "service": "anthropic" }],
///     "mcp": [{ "kind": "host", "name": "tools", "port": 47813 }]
/// }))?;
/// let built = SandboxSpec::agent(Agent::Claude)
///     .with_arg("model", "claude-haiku-5-5")
///     .with_workspace("/srv/project")
///     .with_network_deny(["*.pastebin.com"])
///     .with_key(KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic"))
///     .with_mcp(McpServer::host("tools", 47813));
/// assert_eq!(spec, built);
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSpec {
    /// Agent whose kit is the base kit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<Agent>,
    /// Published base kit, by its reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base: Option<String>,
    /// Arguments of the base kit.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) args: BTreeMap<String, String>,
    /// Mixins of the program.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) mixins: Vec<MixinSpec>,
    /// Primary workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<WorkspaceSpec>,
    /// Further host directories or files mounted.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "write_mounts",
        deserialize_with = "read_mounts"
    )]
    pub(crate) mounts: Vec<Workspace>,
    /// Network rules of the sandbox.
    #[serde(default, skip_serializing_if = "Network::is_empty")]
    pub(crate) network: Network,
    /// API keys written by the proxy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) keys: Vec<KeySpec>,
    /// MCP servers registered in the agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) mcp: Vec<McpServer>,
    /// Variables of the sandbox, in clear.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) env: BTreeMap<String, String>,
    /// CPUs and memory.
    #[serde(default, skip_serializing_if = "Resources::is_empty")]
    pub(crate) resources: Resources,
    /// Whether a failed launch keeps the sandbox.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) keep_on_failure: bool,
}

/// The primary workspace of a spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum WorkspaceSpec {
    /// A host directory bind-mounted read-write at its own path.
    Mount(String),
    /// A private clone of the host repository at that path.
    Clone(String),
}

/// The JSON form of a further mount.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MountForm {
    /// Directory or file of the host.
    dir: String,
    /// Whether the sandbox can only read it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    read_only: bool,
    /// Path inside the sandbox, the one of `dir` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    at: Option<String>,
}

/// The resources of a spec.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Resources {
    /// Number of virtual CPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cpus: Option<u32>,
    /// Memory limit, in the notation of `sbx` (`4g`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) memory: Option<String>,
}

impl SandboxSpec {
    /// A sandbox of the kit of `agent`, resolved as `sbx create <agent>`
    /// resolves it.
    #[must_use]
    pub fn agent(agent: Agent) -> Self {
        Self {
            agent: Some(agent),
            ..Self::empty()
        }
    }

    /// A sandbox of the published kit `reference`, e.g.
    /// `docker.io/sbx/shell:latest`.
    #[must_use]
    pub fn base(reference: impl Into<String>) -> Self {
        Self {
            base: Some(reference.into()),
            ..Self::empty()
        }
    }

    /// A spec of no kit, which the constructors complete.
    fn empty() -> Self {
        Self {
            agent: None,
            base: None,
            args: BTreeMap::new(),
            mixins: Vec::new(),
            workspace: None,
            mounts: Vec::new(),
            network: Network::default(),
            keys: Vec::new(),
            mcp: Vec::new(),
            env: BTreeMap::new(),
            resources: Resources::default(),
            keep_on_failure: false,
        }
    }

    /// Gives the value `value` to the argument `name` of the base kit.
    #[must_use]
    pub fn with_arg(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.args.insert(name.into(), value.into());
        self
    }

    /// Bind-mounts the host directory `dir`, by its real absolute path, as
    /// the workspace, read-write at the same path inside the sandbox.
    #[must_use]
    pub fn with_workspace(mut self, dir: impl Into<String>) -> Self {
        self.workspace = Some(WorkspaceSpec::Mount(dir.into()));
        self
    }

    /// Runs the agent on a private clone of the Git repository at
    /// `repository`, as `sbx create --clone` does.
    #[must_use]
    pub fn with_cloned_workspace(mut self, repository: impl Into<String>) -> Self {
        self.workspace = Some(WorkspaceSpec::Clone(repository.into()));
        self
    }

    /// Bind-mounts a further host directory or file.
    #[must_use]
    pub fn with_mount(mut self, mount: Workspace) -> Self {
        self.mounts.push(mount);
        self
    }

    /// Sets the variable `key` of the sandbox to `value`, in clear in the
    /// creation request: a secret goes in a key instead.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Sets the number of virtual CPUs.
    #[must_use]
    pub fn with_cpus(mut self, cpus: u32) -> Self {
        self.resources.cpus = Some(cpus);
        self
    }

    /// Sets the memory limit, in the notation of `sbx` (`4g`).
    #[must_use]
    pub fn with_memory(mut self, memory: impl Into<String>) -> Self {
        self.resources.memory = Some(memory.into());
        self
    }

    /// Keeps the sandbox when a launch fails after its creation, for a look
    /// inside: the error names it.
    #[must_use]
    pub fn keep_on_failure(mut self) -> Self {
        self.keep_on_failure = true;
        self
    }
}

impl Resources {
    /// Whether the spec names no resource.
    fn is_empty(&self) -> bool {
        self.cpus.is_none() && self.memory.is_none()
    }
}

/// Writes the further mounts as `{"dir", "read_only"?, "at"?}`.
fn write_mounts<S: Serializer>(mounts: &[Workspace], serializer: S) -> Result<S::Ok, S::Error> {
    let forms: Vec<MountForm> = mounts
        .iter()
        .map(|mount| MountForm {
            dir: mount.dir.clone(),
            read_only: mount.read_only,
            at: mount.target.clone(),
        })
        .collect();
    forms.serialize(serializer)
}

/// Reads the further mounts from `{"dir", "read_only"?, "at"?}`.
fn read_mounts<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Workspace>, D::Error> {
    let forms = Vec::<MountForm>::deserialize(deserializer)?;
    Ok(forms
        .into_iter()
        .map(|form| Workspace {
            dir: form.dir,
            read_only: form.read_only,
            target: form.at,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use docker_sandbox_kit::{KitDescriptor, Mixin};
    use serde_json::json;

    use super::*;

    /// A spec with every field.
    fn whole() -> SandboxSpec {
        let tools = Mixin::new("/kits/tools", KitDescriptor::mixin("Tools")).unwrap();
        SandboxSpec::agent(Agent::Claude)
            .with_arg("model", "m")
            .with_mixin(tools)
            .with_mixin_dir("/kits/acme")
            .with_cloned_workspace("/srv/repo")
            .with_mount(Workspace::new("/srv/docs").read_only().at("/docs"))
            .with_mount(Workspace::new("/srv/data"))
            .with_network_allow(["api.example.com"])
            .with_network_deny(["*.pastebin.com"])
            .with_key(KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic"))
            .with_mcp(McpServer::host("tools", 47813))
            .with_env("LOG", "debug")
            .with_cpus(2)
            .with_memory("4g")
            .keep_on_failure()
    }

    #[test]
    fn specs_are_written_and_read_as_json() {
        let spec = whole();
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["agent"], "claude");
        assert_eq!(json["mixins"][1], json!({ "dir": "/kits/acme" }));
        assert_eq!(json["mixins"][0]["reference"], "/kits/tools");
        assert_eq!(json["workspace"], json!({ "clone": "/srv/repo" }));
        assert_eq!(
            json["mounts"],
            json!([{ "dir": "/srv/docs", "read_only": true, "at": "/docs" }, { "dir": "/srv/data" }])
        );
        assert_eq!(json["resources"], json!({ "cpus": 2, "memory": "4g" }));
        assert_eq!(json["keep_on_failure"], true);
        assert_eq!(serde_json::from_value::<SandboxSpec>(json).unwrap(), spec);

        let base = SandboxSpec::base("docker.io/sbx/shell:latest").with_workspace("/srv/project");
        let json = serde_json::to_value(&base).unwrap();
        assert_eq!(
            json,
            json!({ "base": "docker.io/sbx/shell:latest", "workspace": { "mount": "/srv/project" } })
        );
        assert_eq!(serde_json::from_value::<SandboxSpec>(json).unwrap(), base);
    }

    #[test]
    fn unknown_fields_are_refused() {
        for refused in [
            json!({ "agent": "claude", "name": "job-42" }),
            json!({ "agent": "claude", "mixins": [{ "dir": "/kits/acme", "name": "acme" }] }),
            json!({ "agent": "claude", "mixins": [{ "reference": "/kits/acme" }] }),
            json!({ "agent": "claude", "workspace": { "mount": "/srv", "read_only": true } }),
            json!({ "agent": "claude", "mounts": [{ "dir": "/srv", "target": "/x" }] }),
            json!({ "agent": "claude", "network": { "allowed": [] } }),
            json!({ "agent": "claude", "resources": { "gpus": 1 } }),
            json!({ "agent": "acme" }),
        ] {
            assert!(
                serde_json::from_value::<SandboxSpec>(refused.clone()).is_err(),
                "{refused}"
            );
        }
    }
}
