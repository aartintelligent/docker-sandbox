//! Sandbox creation (`POST /sandbox`).
//!
//! The daemon cannot resolve kit references by itself: the `kit_artifacts` of
//! a creation request are assembled by the `sbx` CLI. A request is therefore
//! obtained from a template captured once from the CLI, usually through a
//! [`KitPackage`](crate::kit::KitPackage), then adjusted with the `with_*`
//! methods.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{PortBinding, SandboxState};
use crate::Extra;
use crate::error::{Error, Result};
use crate::mcp::McpGateway;
use crate::transport::NdjsonStream;

/// Progress stream of a sandbox creation, returned by
/// [`Sandboxes::create_stream`](super::Sandboxes::create_stream).
pub type CreateStream = NdjsonStream<CreateEvent>;

/// Body of `POST /sandbox`.
///
/// `kit_artifacts` is treated as opaque. Start from
/// [`KitPackage::request`](crate::kit::KitPackage::request),
/// [`CreateRequest::from_template`] or [`CreateRequest::for_agent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateRequest {
    /// Sandbox name.
    pub name: String,
    /// Agent of the base kit, e.g. `shell`.
    pub agent: String,
    /// Kit references, base kit first.
    #[serde(default)]
    pub kits: Vec<String>,
    /// Resolved kit artifacts, one per entry of `kits`. Opaque.
    #[serde(default)]
    pub kit_artifacts: Vec<Value>,
    /// When to pull kit images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_policy: Option<PullPolicy>,
    /// Image the sandbox starts from instead of the image of its agent, such
    /// as one saved with [`Sandbox::save`](super::Sandbox::save).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Number of virtual CPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    /// Memory limit in `sbx` notation, e.g. `4g`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    /// Host directory to bind-mount as workspace, at the same path inside the
    /// sandbox: its working directory. Without one, the agent works in the
    /// filesystem of the container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Further host directories or files to bind-mount.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_workspaces: Vec<Workspace>,
    /// Whether the agent works on a private clone of the Git repository of
    /// `workspace` instead of the directory itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clone: bool,
    /// Environment variables of the sandbox.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    /// Ports published from the start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortBinding>,
    /// How the shared skills store of the host reaches the skills directory of
    /// the agent. The daemon default applies when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<SkillsMode>,
    /// Governance profile of the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// MCP gateway configuration, e.g. `{"servers": [], "static": true}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<Value>,
    /// Host file binding stored secrets to the credentials the kits declare
    /// (`~/.config/sbx/credentials.yaml` for the CLI, which records there what
    /// the user approves). The daemon reads it to authorize the credentials
    /// the kits declare; see [`CreateRequest::with_bindings_path`] for those
    /// it authorizes without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bindings_path: Option<String>,
    /// Host SSH agent socket to forward. The daemon checks that it speaks the
    /// SSH agent protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_agent_socket_path: Option<String>,
    /// Enables the virtio-fs cache for workspace mounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_virtiofs_cache: Option<bool>,
    /// Any other field, forwarded unchanged.
    #[serde(flatten)]
    pub extra: Extra,
}

impl CreateRequest {
    /// Builds a request from a captured template, replacing its name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if `template` is not a valid creation
    /// body.
    ///
    /// ```
    /// use docker_sandbox::sandbox::CreateRequest;
    ///
    /// let template = serde_json::json!({
    ///     "name": "{{name}}",
    ///     "agent": "shell",
    ///     "kits": ["docker.io/sbx/shell@sha256:f3c1"],
    ///     "kit_artifacts": [{}],
    ///     "pull_policy": "missing"
    /// });
    /// let request = CreateRequest::from_template(template, "job-42")?
    ///     .with_cpus(2)
    ///     .with_memory("4g")
    ///     .with_env("CI", "true")
    ///     .with_static_mcp_servers(["docs"]);
    /// assert_eq!(request.name, "job-42");
    /// assert_eq!(request.mcp, Some(serde_json::json!({ "servers": ["docs"], "static": true })));
    /// # Ok::<(), docker_sandbox::Error>(())
    /// ```
    pub fn from_template(template: Value, name: impl Into<String>) -> Result<Self> {
        let mut request: Self = serde_json::from_value(template)
            .map_err(|e| Error::InvalidRequest(format!("invalid creation template: {e}")))?;
        request.name = name.into();
        Ok(request)
    }

    /// A request for the sandbox `name` whose agent, the name of its base kit,
    /// is `agent`, with the options the CLI gives a new sandbox: kit images
    /// pulled when missing, the virtiofs cache on. Its kits and their
    /// artifacts are for the caller to set, as a kit forger does.
    ///
    /// ```
    /// use docker_sandbox::sandbox::{CreateRequest, PullPolicy};
    ///
    /// let request = CreateRequest::for_agent("job-42", "claude");
    /// assert_eq!((request.name.as_str(), request.agent.as_str()), ("job-42", "claude"));
    /// assert_eq!(request.pull_policy, Some(PullPolicy::Missing));
    /// assert!(request.kits.is_empty() && request.kit_artifacts.is_empty());
    /// ```
    #[must_use]
    pub fn for_agent(name: impl Into<String>, agent: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            agent: agent.into(),
            kits: Vec::new(),
            kit_artifacts: Vec::new(),
            pull_policy: Some(PullPolicy::Missing),
            template: None,
            cpus: None,
            memory: None,
            workspace: None,
            additional_workspaces: Vec::new(),
            clone: false,
            environment: BTreeMap::new(),
            ports: Vec::new(),
            skills: None,
            profile: None,
            mcp: None,
            bindings_path: None,
            ssh_agent_socket_path: None,
            enable_virtiofs_cache: Some(true),
            extra: Extra::new(),
        }
    }

    /// Bind-mounts the host directory `dir` as workspace, read-write, at the
    /// same path inside the sandbox: the agent changes the files of the host.
    /// Give its real absolute path, as `std::fs::canonicalize` returns it: a
    /// `..` in it names nothing inside the sandbox.
    #[must_use]
    pub fn with_workspace(mut self, dir: impl Into<String>) -> Self {
        self.workspace = Some(dir.into());
        self
    }

    /// Bind-mounts a further host directory or file, as
    /// `sbx create AGENT PATH… DIR[:TARGET][:ro]` does.
    ///
    /// ```
    /// use docker_sandbox::sandbox::{CreateRequest, Workspace};
    ///
    /// # let template = serde_json::json!({ "name": "{{name}}", "agent": "shell" });
    /// let request = CreateRequest::from_template(template, "job-42")?
    ///     .with_workspace("/srv/project")
    ///     .with_additional_workspace(Workspace::new("/srv/docs").read_only())
    ///     .with_additional_workspace(Workspace::new("/srv/data").at("/workspace/data"));
    /// assert!(request.additional_workspaces[0].read_only);
    /// # Ok::<(), docker_sandbox::Error>(())
    /// ```
    #[must_use]
    pub fn with_additional_workspace(mut self, workspace: Workspace) -> Self {
        self.additional_workspaces.push(workspace);
        self
    }

    /// Runs the agent on a private clone of the Git repository at `repository`,
    /// as `sbx create --clone` does: the repository is mounted read-only at
    /// `/run/sandbox/source`, the clone sits at its path, on the ref checked
    /// out on the host, and the commits of the agent stay in the sandbox.
    ///
    /// The CLI brings them back with `git fetch sandbox-<name>`, through a
    /// remote it adds to the host repository: `git upload-pack` run in the
    /// sandbox by [`Sandbox::attach`](crate::sandbox::Sandbox::attach).
    #[must_use]
    pub fn with_cloned_workspace(mut self, repository: impl Into<String>) -> Self {
        self.workspace = Some(repository.into());
        self.clone = true;
        self
    }

    /// Publishes a port from the start, as `sbx create -p` does; see
    /// [`Sandbox::publish`](crate::sandbox::Sandbox::publish) for later ones.
    #[must_use]
    pub fn with_port(mut self, port: PortBinding) -> Self {
        self.ports.push(port);
        self
    }

    /// Chooses how the shared skills store reaches the agent (`--skills`).
    #[must_use]
    pub fn with_skills(mut self, mode: SkillsMode) -> Self {
        self.skills = Some(mode);
        self
    }

    /// Assigns the governance profile `profile` (`sbx policy profile ls`). The
    /// daemon refuses an unknown profile, before creating anything.
    #[must_use]
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// Records the credential bindings file of the host, as the CLI does with
    /// `~/.config/sbx/credentials.yaml`. The daemon reads it to authorize the
    /// credentials the kits declare. Without one, as the audit measured, it
    /// still authorizes the credential of a published kit whose artifact is
    /// the one the CLI resolves, `v3Effective` in the order of the CLI
    /// included: a `claude` sandbox then uses the credential stored for
    /// `anthropic`, a subscription for instance. It withholds the others
    /// ([`Created::withheld`]), such as the credential a mixin declares for
    /// its own service, and a value pushed later for their service does not
    /// lift that.
    ///
    /// A program that gives an agent a key of its own, without a bindings
    /// file, uses a [`ProxiedKey`](super::ProxiedKey): custom secrets do not
    /// depend on bindings.
    #[must_use]
    pub fn with_bindings_path(mut self, path: impl Into<String>) -> Self {
        self.bindings_path = Some(path.into());
        self
    }

    /// Sets the number of virtual CPUs.
    #[must_use]
    pub fn with_cpus(mut self, cpus: u32) -> Self {
        self.cpus = Some(cpus);
        self
    }

    /// Sets the memory limit (`sbx` notation, e.g. `4g`).
    #[must_use]
    pub fn with_memory(mut self, memory: impl Into<String>) -> Self {
        self.memory = Some(memory.into());
        self
    }

    /// Adds an environment variable of the sandbox.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    /// Adds environment variables of the sandbox.
    #[must_use]
    pub fn with_envs<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.environment
            .extend(vars.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Sets when kit images are pulled.
    #[must_use]
    pub fn with_pull_policy(mut self, policy: PullPolicy) -> Self {
        self.pull_policy = Some(policy);
        self
    }

    /// Starts the sandbox from the image `image` instead of the image of its
    /// agent, as `sbx create -t IMAGE` does: a sandbox saved with
    /// [`Sandbox::save`](super::Sandbox::save) comes back with its changes.
    /// The CLI pairs it with [`PullPolicy::Never`] for an image saved on this
    /// host.
    ///
    /// ```
    /// use docker_sandbox::sandbox::{CreateRequest, PullPolicy};
    ///
    /// # let template = serde_json::json!({ "name": "{{name}}", "agent": "shell" });
    /// let request = CreateRequest::from_template(template, "job-43")?
    ///     .with_template("job-42-snapshot:v1")
    ///     .with_pull_policy(PullPolicy::Never);
    /// let body = serde_json::to_value(&request).unwrap();
    /// assert_eq!(body["template"], "job-42-snapshot:v1");
    /// assert_eq!(body["pull_policy"], "never");
    /// # Ok::<(), docker_sandbox::Error>(())
    /// ```
    #[must_use]
    pub fn with_template(mut self, image: impl Into<String>) -> Self {
        self.template = Some(image.into());
        self
    }

    /// Forwards the host SSH agent socket at `path`.
    #[must_use]
    pub fn with_ssh_agent_socket(mut self, path: impl Into<String>) -> Self {
        self.ssh_agent_socket_path = Some(path.into());
        self
    }

    /// Fixes the MCP servers of the sandbox at creation, as
    /// `sbx create --static-mcp` does: `servers` are names registered with
    /// `sbx mcp add`, loaded into the gateway from the start. An empty list
    /// gives a static gateway without servers.
    #[must_use]
    pub fn with_static_mcp_servers(
        mut self,
        servers: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let servers: Vec<String> = servers.into_iter().map(Into::into).collect();
        self.mcp = Some(serde_json::json!({ "servers": servers, "static": true }));
        self
    }
}

/// When kit images are pulled during creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PullPolicy {
    /// Always pull.
    Always,
    /// Pull only images missing locally.
    Missing,
    /// Never pull.
    Never,
}

/// A further host directory or file bind-mounted into a sandbox, see
/// [`CreateRequest::with_additional_workspace`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Directory or file on the host, by its real absolute path.
    pub dir: String,
    /// Whether the sandbox can only read it. A read-only file inside a
    /// writable workspace stays out of reach of the agent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// Absolute path inside the sandbox; the path of `dir` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl Workspace {
    /// Mounts `dir` read-write at its own path.
    pub fn new(dir: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            read_only: false,
            target: None,
        }
    }

    /// Mounts it read-only (`:ro`).
    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Mounts it at the absolute path `target` of the sandbox (`:TARGET`).
    #[must_use]
    pub fn at(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }
}

/// How the shared skills store of the host (`sbx skills add`) reaches the
/// skills directory of the agent, e.g. `~/.claude/skills`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkillsMode {
    /// Not at all.
    Off,
    /// Linked in read-only; the directory stays writable.
    Readonly,
    /// Mounted over the directory: what the agent writes is shared.
    Readwrite,
}

/// Outcome of a successful creation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Created {
    /// Identifier assigned by the daemon.
    pub id: String,
    /// Sandbox name.
    pub name: String,
    /// Agent of the base kit.
    pub agent: String,
    /// Runtime state right after creation.
    pub status: SandboxState,
    /// Workspace directory, empty when there is none.
    #[serde(default)]
    pub workspace: String,
    /// MCP gateway of the sandbox.
    #[serde(default)]
    pub mcp: Option<McpGateway>,
    /// Services whose credential, declared by a kit of the sandbox, the
    /// daemon does not authorize ([`CreateRequest::with_bindings_path`] says
    /// which it does): the proxy injects nothing for them. Empty when the kits
    /// declare no credential or every one is authorized.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// One line of the NDJSON stream returned by `POST /sandbox`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CreateEvent {
    /// Human-readable progress message.
    Progress(String),
    /// Creation succeeded. Always the last event of a successful stream.
    ///
    /// Boxed, so that every event stays as small as a progress line however
    /// large [`Created`] grows with the daemon: its fields read through the
    /// box, and `*created` moves the value out.
    Result(Box<Created>),
    /// Creation failed. Always the last event of a failed stream.
    Error(String),
    /// A line this crate does not know, kept for inspection.
    Unknown(Value),
}

impl CreateEvent {
    /// Parses one NDJSON line. Returns `None` for blank lines.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if the line is not valid JSON, or if a
    /// `result` line does not describe a sandbox.
    pub fn parse_line(line: &str) -> Result<Option<Self>> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(None);
        }
        let decode = |source| Error::Decode {
            operation: "sandbox creation",
            source,
        };
        let mut value: Value = serde_json::from_str(line).map_err(decode)?;
        let event = if let Some(result) = value.get_mut("result") {
            Self::Result(Box::new(
                serde_json::from_value(result.take()).map_err(decode)?,
            ))
        } else if let Some(error) = value.get("error") {
            Self::Error(
                error
                    .as_str()
                    .map_or_else(|| error.to_string(), str::to_owned),
            )
        } else if let Some(progress) = value.get("progress").and_then(Value::as_str) {
            Self::Progress(progress.trim_end().to_owned())
        } else {
            Self::Unknown(value)
        };
        Ok(Some(event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_lines_are_classified() {
        let progress = CreateEvent::parse_line(r#"{"progress":"Pulling\n"}"#).unwrap();
        assert_eq!(progress, Some(CreateEvent::Progress("Pulling".to_owned())));

        let failure = CreateEvent::parse_line(r#"{"error":"image is not cached"}"#).unwrap();
        assert_eq!(
            failure,
            Some(CreateEvent::Error("image is not cached".to_owned()))
        );

        let result = CreateEvent::parse_line(
            r#"{"result":{"id":"1","name":"a","agent":"shell","status":"running"}}"#,
        )
        .unwrap();
        assert!(matches!(result, Some(CreateEvent::Result(c)) if c.name == "a"));
        assert!(CreateEvent::parse_line(" \n").unwrap().is_none());
        assert!(matches!(
            CreateEvent::parse_line(r#"{"other":1}"#).unwrap(),
            Some(CreateEvent::Unknown(_))
        ));
    }
}
