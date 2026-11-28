//! Kit descriptors: what a `spec.yaml` holds, read from JSON or built in Rust.
//!
//! The daemon reads the v3 descriptor of every kit it composes and derives the
//! permissions, network rules and commands of the sandbox from it. A forged
//! mixin may declare what the audit measured the daemon accepting from one:
//! `lifecycle@1` install and startup commands, and `network-policy@1` rules.
//! Anything else is refused rather than sent on, so that a forged kit never
//! half succeeds in silence.
//!
//! The descriptor is a typed model read and written by serde. Its lifecycle
//! steps and network rules are also how the configurations of published kits
//! are read: there, fields this crate does not know are kept (`extra`); in a
//! mixin to forge, they are refused.
//!
//! Descriptors are JSON. A `spec.yaml` written in the JSON subset of YAML,
//! after its `# syntax=docker/sandbox-kit:3` line, is read as it is, and the
//! `sbx` CLI reads it too.

use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::to_json;
use crate::error::{Error, Result};
use docker_sandbox::Extra;
use docker_sandbox::policy::HostPattern;

/// First line of a v3 descriptor: the `BuildKit` frontend that reads it.
const SYNTAX_LINE: &str = "# syntax=docker/sandbox-kit:3";

/// Capability of the install and startup commands, the `type` of
/// [`Capability::Lifecycle`].
pub(super) const LIFECYCLE: &str = "com.docker.sandbox/lifecycle@1";

/// Capability of the network rules, the `type` of
/// [`Capability::NetworkPolicy`].
pub(super) const NETWORK: &str = "com.docker.sandbox/network-policy@1";

/// The variables of the sandbox that name its MCP gateway, measured on the
/// commands of the kits (`audit-mixin-mcp`).
const GATEWAY_ENV: [&str; 2] = ["MCP_GATEWAY_URL", "MCP_SENTINEL_TOKEN_NAME"];
/// A v3 kit descriptor (feature `forge`), read from JSON or built in Rust.
///
/// Its JSON form (serde) is the descriptor itself, the JSON of a `spec.yaml`,
/// read and checked as [`KitDescriptor::from_json`] reads it.
///
/// ```
/// use docker_sandbox_kit::{KitDescriptor, LifecycleStep};
///
/// let descriptor = KitDescriptor::mixin("Acme tools")
///     .with_description("Tools for the Acme API")
///     .with_network_allow(["api.acme.example:443"])
///     .with_install(LifecycleStep::new("pip install acme-cli").as_user("agent"));
/// let spec = descriptor.to_spec();
/// assert!(spec.starts_with("# syntax=docker/sandbox-kit:3\n"));
/// assert_eq!(KitDescriptor::parse(&spec)?, descriptor);
/// # Ok::<(), docker_sandbox_kit::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct KitDescriptor {
    /// The fields of the descriptor.
    spec: Spec,
}

/// The fields of a v3 mixin descriptor, in the order `spec.yaml` writes them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Spec {
    /// Version of the descriptor format.
    pub(super) schema_version: SchemaVersion,
    /// Kind of kit.
    pub(super) kind: MixinKind,
    /// Name shown for the kit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) display_name: Option<String>,
    /// What the kit is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    /// Version of the kit, e.g. `1.0.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) version: Option<String>,
    /// Author of the kit, kept as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author: Option<Value>,
    /// Licenses of the kit, kept as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    licenses: Option<Value>,
    /// Where the kit comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    /// What the kit declares, in order.
    #[serde(default)]
    pub(super) capabilities: Vec<Capability>,
}

/// The version of the descriptor format this crate forges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum SchemaVersion {
    /// Kits v3.
    #[default]
    #[serde(rename = "3")]
    V3,
}

/// The kind of kit this crate forges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum MixinKind {
    /// A kit composed on top of a base kit.
    #[serde(rename = "mixin")]
    Mixin,
}

/// A capability a forged mixin may declare.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub(super) enum Capability {
    /// Install and startup commands ([`LIFECYCLE`]).
    #[serde(rename = "com.docker.sandbox/lifecycle@1")]
    Lifecycle {
        /// Name of the capability.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// What the capability does.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Whether the kit works without it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        optional: Option<bool>,
        /// The commands.
        #[serde(default)]
        config: LifecycleConfig,
    },
    /// Network rules ([`NETWORK`]).
    #[serde(rename = "com.docker.sandbox/network-policy@1")]
    NetworkPolicy {
        /// Name of the capability.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// What the capability does.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Whether the kit works without it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        optional: Option<bool>,
        /// The rules.
        #[serde(default)]
        config: NetworkConfig,
    },
}

/// The configuration of a `lifecycle@1` capability.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct LifecycleConfig {
    /// Commands run once, when the sandbox is created.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) install: Vec<LifecycleStep>,
    /// Commands run at every start of the sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) startup: Vec<LifecycleStep>,
    /// Phases this crate does not know: kept from a published kit, refused in
    /// a mixin.
    #[serde(flatten)]
    pub(super) extra: Extra,
}

/// The configuration of a `network-policy@1` capability.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct NetworkConfig {
    /// Rules while the sandbox runs.
    #[serde(default, skip_serializing_if = "NetworkRules::is_empty")]
    pub(super) runtime: NetworkRules,
    /// Rules while the install commands run.
    #[serde(default, skip_serializing_if = "NetworkRules::is_empty")]
    pub(super) install: NetworkRules,
    /// Phases this crate does not know: kept from a published kit, refused in
    /// a mixin.
    #[serde(flatten)]
    pub(super) extra: Extra,
}

/// Hosts allowed and denied, each `host` or `host:port`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct NetworkRules {
    /// Hosts allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) allow: Vec<String>,
    /// Hosts denied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) deny: Vec<String>,
    /// Decisions this crate does not know: kept from a published kit, refused
    /// in a mixin.
    #[serde(flatten)]
    pub(super) extra: Extra,
}

/// The command of a lifecycle step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum Command {
    /// A shell command, run with `sh -c`.
    Shell(String),
    /// A program and its arguments.
    Exec(Vec<String>),
}

impl KitDescriptor {
    /// An empty mixin named `display_name`, to build on with the `with_*`
    /// methods.
    pub fn mixin(display_name: impl Into<String>) -> Self {
        Self {
            spec: Spec {
                schema_version: SchemaVersion::V3,
                kind: MixinKind::Mixin,
                display_name: Some(display_name.into()),
                description: None,
                version: None,
                author: None,
                licenses: None,
                source_url: None,
                capabilities: Vec::new(),
            },
        }
    }

    /// A descriptor from its JSON.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidKit`] when it is not a v3 mixin this crate can
    /// forge.
    pub fn from_json(value: Value) -> Result<Self> {
        let kit = value
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or("descriptor")
            .to_owned();
        let spec = serde_json::from_value(value).map_err(|error| Error::InvalidKit {
            kit,
            message: format!("not a v3 mixin this crate forges: {error}"),
        })?;
        let descriptor = Self { spec };
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// A descriptor from the text of a `spec.yaml` written in JSON: the
    /// comment and blank lines that open it, such as its `# syntax=` line,
    /// are skipped.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidKit`] when the rest is not JSON, or not a v3
    /// mixin this crate can forge.
    pub fn parse(text: &str) -> Result<Self> {
        let body: Vec<&str> = text
            .lines()
            .skip_while(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
            .collect();
        let value = serde_json::from_str(&body.join("\n")).map_err(|e| Error::InvalidKit {
            kit: "descriptor".to_owned(),
            message: format!("not JSON ({e}); a spec.yaml is read in the JSON subset of YAML"),
        })?;
        Self::from_json(value)
    }

    /// The descriptor in the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Client`] ([`docker_sandbox::Error::File`]) when the file cannot be read, and
    /// [`Error::InvalidKit`] as [`KitDescriptor::parse`] does.
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        Self::read_file(path.as_ref())
    }

    /// [`KitDescriptor::read`], compiled once whatever the type of path.
    fn read_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| docker_sandbox::Error::File {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Sets the description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.spec.description = Some(description.into());
        self
    }

    /// Sets the version of the kit, e.g. `1.0.0`.
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.spec.version = Some(version.into());
        self
    }

    /// Adds a command run once, when the sandbox is created, before its agent
    /// starts.
    #[must_use]
    pub fn with_install(mut self, step: LifecycleStep) -> Self {
        self.edit_lifecycle(|config| config.install.push(step));
        self
    }

    /// Adds a command run at every start of the sandbox, alongside its agent.
    #[must_use]
    pub fn with_startup(mut self, step: LifecycleStep) -> Self {
        self.edit_lifecycle(|config| config.startup.push(step));
        self
    }

    /// Allows `hosts` while the sandbox runs, in the forms of
    /// [`HostPattern`] (`host`, `*.domain`, `host:port`, a CIDR prefix…),
    /// checked when the mixin is made.
    #[must_use]
    pub fn with_network_allow(
        mut self,
        hosts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let hosts = hosts.into_iter().map(Into::into);
        self.edit_network(|config| config.runtime.allow.extend(hosts));
        self
    }

    /// Denies `hosts` while the sandbox runs, in the forms of
    /// [`HostPattern`], checked when the mixin is made. A deny wins over any
    /// allow, of the sandbox or of its kits.
    #[must_use]
    pub fn with_network_deny(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let hosts = hosts.into_iter().map(Into::into);
        self.edit_network(|config| config.runtime.deny.extend(hosts));
        self
    }

    /// The display name, if the descriptor has one.
    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        self.spec.display_name.as_deref()
    }

    /// The descriptor as JSON.
    #[must_use]
    pub fn to_json(&self) -> Value {
        to_json(&self.spec)
    }

    /// The text of a `spec.yaml` holding the descriptor: its `# syntax=` line,
    /// then the descriptor as JSON, which the `sbx` CLI reads as YAML.
    #[must_use]
    pub fn to_spec(&self) -> String {
        // Serializing the descriptor cannot fail: its maps have string keys.
        let json = serde_json::to_string_pretty(&self.spec).unwrap_or_default();
        format!("{SYNTAX_LINE}\n{json}\n")
    }

    /// The fields of the descriptor.
    pub(super) fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Applies `change` to the configuration of the lifecycle capability,
    /// appended when the descriptor has none.
    fn edit_lifecycle(&mut self, change: impl FnOnce(&mut LifecycleConfig)) {
        for capability in &mut self.spec.capabilities {
            if let Capability::Lifecycle { config, .. } = capability {
                change(config);
                return;
            }
        }
        let mut config = LifecycleConfig::default();
        change(&mut config);
        self.spec.capabilities.push(Capability::Lifecycle {
            name: None,
            description: None,
            optional: None,
            config,
        });
    }

    /// Applies `change` to the configuration of the network capability,
    /// appended when the descriptor has none.
    fn edit_network(&mut self, change: impl FnOnce(&mut NetworkConfig)) {
        for capability in &mut self.spec.capabilities {
            if let Capability::NetworkPolicy { config, .. } = capability {
                change(config);
                return;
            }
        }
        let mut config = NetworkConfig::default();
        change(&mut config);
        self.spec.capabilities.push(Capability::NetworkPolicy {
            name: None,
            description: None,
            optional: None,
            config,
        });
    }

    /// Checks what the types of the descriptor leave open: that it uses no
    /// kit argument, names no field this crate does not know, and that its
    /// steps and rules are ones the daemon applies.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidKit`], naming the kit and what is wrong.
    pub(super) fn validate(&self) -> Result<()> {
        self.spec.check().map_err(|message| Error::InvalidKit {
            kit: self.display_name().unwrap_or("descriptor").to_owned(),
            message,
        })
    }
}

impl Serialize for KitDescriptor {
    /// Writes the descriptor as its JSON form, [`KitDescriptor::to_json`].
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.spec.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for KitDescriptor {
    /// Reads a descriptor as [`KitDescriptor::from_json`] does, checked as
    /// it checks it.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Value::deserialize(deserializer)
            .and_then(|value| Self::from_json(value).map_err(serde::de::Error::custom))
    }
}

impl Spec {
    /// The configurations of its lifecycle capabilities.
    pub(super) fn lifecycles(&self) -> impl Iterator<Item = &LifecycleConfig> {
        self.capabilities
            .iter()
            .filter_map(|capability| match capability {
                Capability::Lifecycle { config, .. } => Some(config),
                Capability::NetworkPolicy { .. } => None,
            })
    }

    /// The configurations of its network capabilities.
    pub(super) fn network_policies(&self) -> impl Iterator<Item = &NetworkConfig> {
        self.capabilities
            .iter()
            .filter_map(|capability| match capability {
                Capability::NetworkPolicy { config, .. } => Some(config),
                Capability::Lifecycle { .. } => None,
            })
    }

    /// What is wrong with the descriptor as a mixin to forge, if anything.
    fn check(&self) -> std::result::Result<(), String> {
        if to_json(self).to_string().contains("${{") {
            return Err("kit arguments are not supported in forged mixins".to_owned());
        }
        for (index, capability) in self.capabilities.iter().enumerate() {
            match capability {
                Capability::Lifecycle { config, .. } => config.check(),
                Capability::NetworkPolicy { config, .. } => config.check(),
            }
            .map_err(|error| format!("capabilities[{index}]: {error}"))?;
        }
        Ok(())
    }
}

impl Capability {
    /// The `type` of the capability.
    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::Lifecycle { .. } => LIFECYCLE,
            Self::NetworkPolicy { .. } => NETWORK,
        }
    }
}

impl LifecycleConfig {
    /// What is wrong with the commands of a mixin, if anything: every step
    /// has a command, so that none is dropped in silence by the daemon.
    fn check(&self) -> std::result::Result<(), String> {
        unknown(&self.extra, "lifecycle phase")?;
        for (phase, steps) in [("install", &self.install), ("startup", &self.startup)] {
            for (index, step) in steps.iter().enumerate() {
                step.check(phase)
                    .map_err(|error| format!("{phase}[{index}]: {error}"))?;
            }
        }
        Ok(())
    }
}

impl NetworkConfig {
    /// What is wrong with the rules of a mixin, if anything: rules at run
    /// time, and allowed hosts during installation, each host in a form the
    /// CLI accepts ([`HostPattern`]): the daemon would store a URL or a path
    /// as the host of a rule, and never apply it.
    fn check(&self) -> std::result::Result<(), String> {
        unknown(&self.extra, "network phase")?;
        for (phase, rules) in [("runtime", &self.runtime), ("install", &self.install)] {
            unknown(&rules.extra, &format!("{phase} decision"))?;
            if phase == "install" && !rules.deny.is_empty() {
                return Err(
                    "install.deny is not forged: only hosts allowed during installation are"
                        .to_owned(),
                );
            }
            for (decision, hosts) in [("allow", &rules.allow), ("deny", &rules.deny)] {
                for host in hosts {
                    HostPattern::parse(host.as_str())
                        .map_err(|error| format!("{phase}.{decision}: {error}"))?;
                }
            }
        }
        Ok(())
    }
}

impl NetworkRules {
    /// Whether there are no rules at all.
    fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty() && self.extra.is_empty()
    }
}

/// A command a mixin runs when the sandbox is created (install) or at each of
/// its starts (startup).
///
/// ```
/// use docker_sandbox_kit::LifecycleStep;
///
/// let step = LifecycleStep::new("echo ready > /tmp/ready")
///     .as_user("agent")
///     .with_description("Marks the sandbox ready")
///     .with_env(["MCP_GATEWAY_URL"]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleStep {
    /// The command, a shell command run with `sh -c` or a program and its
    /// arguments.
    pub(super) command: Command,
    /// User it runs as, e.g. `agent`, or `0` for root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) user: Option<String>,
    /// What it does, shown as it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    /// Variables of the sandbox it receives: install commands run in an
    /// emptied environment otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) env: Vec<String>,
    /// Whether a startup command runs in the background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) background: Option<bool>,
    /// Fields this crate does not know: kept from a published kit, refused in
    /// a mixin.
    #[serde(flatten)]
    extra: Extra,
}

impl LifecycleStep {
    /// The shell command `command`, run with `sh -c`.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: Command::Shell(command.into()),
            user: None,
            description: None,
            env: Vec::new(),
            background: None,
            extra: Extra::new(),
        }
    }

    /// Runs the command as `user`, e.g. `agent`, or `0` for root.
    #[must_use]
    pub fn as_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Describes what the command does.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Passes the variables `names` of the sandbox to the command, such as
    /// `MCP_GATEWAY_URL`: an install command runs in an emptied environment
    /// that keeps only the proxy and certificate variables otherwise.
    #[must_use]
    pub fn with_env(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.env.extend(names.into_iter().map(Into::into));
        self
    }

    /// Passes the MCP gateway of the sandbox to the command: its address,
    /// `MCP_GATEWAY_URL`, and `MCP_SENTINEL_TOKEN_NAME`, the bearer token the
    /// kits of the agents send it, as they register it for their agent.
    #[must_use]
    pub fn with_gateway_env(self) -> Self {
        self.with_env(GATEWAY_ENV)
    }

    /// Runs a startup command in the background.
    #[must_use]
    pub fn in_background(mut self) -> Self {
        self.background = Some(true);
        self
    }

    /// What is wrong with the step of `phase` of a mixin, if anything.
    fn check(&self, phase: &str) -> std::result::Result<(), String> {
        unknown(&self.extra, "field")?;
        let empty = match &self.command {
            Command::Shell(command) => command.trim().is_empty(),
            Command::Exec(words) => words.is_empty(),
        };
        if empty {
            return Err("no command: the daemon would skip the step without a word".to_owned());
        }
        if let Some(name) = self.env.iter().find(|name| !is_variable_name(name)) {
            return Err(format!("{name} is not a variable name"));
        }
        if self.background == Some(true) && phase != "startup" {
            return Err("only startup commands run in the background".to_owned());
        }
        Ok(())
    }
}

/// An error naming the first field of `extra`, a `what` this crate does not
/// know.
fn unknown(extra: &Extra, what: &str) -> std::result::Result<(), String> {
    match extra.keys().next() {
        Some(key) => Err(format!("unknown {what} {key}")),
        None => Ok(()),
    }
}

/// Whether `name` is the name of an environment variable.
pub(super) fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The message of the `InvalidKit` error `value` gives.
    fn refusal(value: Value) -> String {
        let message = KitDescriptor::from_json(value).unwrap_err().to_string();
        assert!(message.starts_with("invalid kit "), "{message}");
        message
    }

    /// A mixin whose only capability is `capability`.
    fn with(capability: &Value) -> Value {
        json!({ "schemaVersion": "3", "kind": "mixin", "displayName": "Test", "capabilities": [capability] })
    }

    /// Checks that each descriptor of `cases` is refused with a message
    /// holding its expected words.
    fn assert_refused(cases: Vec<(Value, &str)>) {
        for (value, expected) in cases {
            let message = refusal(value.clone());
            assert!(message.contains(expected), "{value}: {message}");
        }
    }

    #[test]
    fn built_mixins_hold_one_capability_per_kind() {
        let descriptor = KitDescriptor::mixin("Acme")
            .with_version("1.0.0")
            .with_description("Acme tools")
            .with_network_allow(["a.example.com:443"])
            .with_network_deny(["b.example.com"])
            .with_install(LifecycleStep::new("echo hi").with_env(["MCP_GATEWAY_URL"]))
            .with_startup(LifecycleStep::new("date").in_background());
        let value = descriptor.to_json();
        assert_eq!(value["capabilities"].as_array().unwrap().len(), 2);
        assert_eq!(value["capabilities"][0]["type"], NETWORK);
        assert_eq!(
            value["capabilities"][0]["config"],
            json!({ "runtime": { "allow": ["a.example.com:443"], "deny": ["b.example.com"] } })
        );
        assert_eq!(value["capabilities"][1]["type"], LIFECYCLE);
        assert_eq!(
            value["capabilities"][1]["config"]["startup"][0],
            json!({ "command": "date", "background": true })
        );
        let kinds: Vec<&str> = descriptor
            .spec()
            .capabilities
            .iter()
            .map(Capability::kind)
            .collect();
        assert_eq!(kinds, [NETWORK, LIFECYCLE]);
        descriptor.validate().unwrap();
        assert_eq!(descriptor.display_name(), Some("Acme"));
    }

    #[test]
    fn read_mixins_without_capabilities_can_be_built_on() {
        let descriptor = KitDescriptor::parse("{\"schemaVersion\": \"3\", \"kind\": \"mixin\"}")
            .unwrap()
            .with_install(
                LifecycleStep::new("echo hi")
                    .as_user("agent")
                    .with_description("d"),
            );
        assert_eq!(
            descriptor.to_json()["capabilities"][0]["config"]["install"][0],
            json!({ "command": "echo hi", "user": "agent", "description": "d" })
        );
    }

    #[test]
    fn specs_are_read_after_their_comment_lines() {
        let spec =
            "# syntax=docker/sandbox-kit:3\n\n{\"schemaVersion\": \"3\", \"kind\": \"mixin\"}\n";
        assert_eq!(KitDescriptor::parse(spec).unwrap().display_name(), None);
        assert!(KitDescriptor::parse("schemaVersion: \"3\"\n").is_err());
    }

    #[test]
    fn specs_are_written_in_the_order_of_their_fields() {
        let spec = KitDescriptor::mixin("Acme").with_version("1.0.0").to_spec();
        let display = spec.find("displayName").unwrap();
        assert!(spec.find("schemaVersion").unwrap() < display);
        assert!(display < spec.find("version\"").unwrap());
        assert!(spec.find("version\"").unwrap() < spec.find("capabilities").unwrap());
    }

    #[test]
    fn what_cannot_be_forged_is_refused() {
        assert_refused(vec![
            (json!([]), "not a v3 mixin"),
            (
                json!({ "schemaVersion": "2", "kind": "mixin" }),
                "unknown variant `2`",
            ),
            (
                json!({ "schemaVersion": "3", "kind": "workload" }),
                "unknown variant `workload`",
            ),
            (
                json!({ "schemaVersion": "3", "kind": "mixin", "extends": "x" }),
                "unknown field `extends`",
            ),
            (
                json!({ "schemaVersion": "3", "kind": "mixin", "capabilities": {} }),
                "expected a sequence",
            ),
            (with(&json!("lifecycle")), "invalid type"),
            (
                with(&json!({ "type": LIFECYCLE, "extra": 1 })),
                "unknown field `extra`",
            ),
            (
                with(&json!({ "type": LIFECYCLE, "config": [] })),
                "invalid type",
            ),
            (with(&json!({ "config": {} })), "missing field `type`"),
            (
                with(&json!({ "type": "com.docker.sandbox/credential@1" })),
                "unknown variant",
            ),
            (
                with(
                    &json!({ "type": LIFECYCLE, "config": { "install": [{ "command": "echo ${{ kit.args.x }}" }] } }),
                ),
                "kit arguments",
            ),
        ]);
        let message = refusal(
            json!({ "schemaVersion": "3", "kind": "mixin", "displayName": "Acme", "x": 1 }),
        );
        assert!(message.starts_with("invalid kit Acme:"), "{message}");
    }

    #[test]
    fn lifecycle_steps_that_cannot_be_forged_are_refused() {
        let step = |phase: &str, step: Value| {
            with(&json!({ "type": LIFECYCLE, "config": { phase: [step] } }))
        };
        assert_refused(vec![
            (
                with(&json!({ "type": LIFECYCLE, "config": { "teardown": [] } })),
                "unknown lifecycle phase teardown",
            ),
            (
                with(&json!({ "type": LIFECYCLE, "config": { "install": {} } })),
                "expected a sequence",
            ),
            (step("install", json!(1)), "invalid type"),
            (
                step("install", json!({ "command": "x", "when": 1 })),
                "install[0]: unknown field when",
            ),
            (
                step("install", json!({ "user": "agent" })),
                "missing field `command`",
            ),
            (step("install", json!({ "command": " " })), "no command"),
            (step("install", json!({ "command": [] })), "no command"),
            (
                step("install", json!({ "command": "x", "user": 0 })),
                "invalid type: integer `0`",
            ),
            (
                step("install", json!({ "command": "x", "env": "A" })),
                "invalid type",
            ),
            (
                step("install", json!({ "command": "x", "env": ["1A"] })),
                "1A is not a variable name",
            ),
            (
                step("install", json!({ "command": "x", "background": true })),
                "only startup",
            ),
            (
                step("startup", json!({ "command": "x", "background": "yes" })),
                "invalid type",
            ),
        ]);
    }

    #[test]
    fn network_rules_that_cannot_be_forged_are_refused() {
        let network = |config: Value| with(&json!({ "type": NETWORK, "config": config }));
        assert_refused(vec![
            (
                network(json!({ "build": {} })),
                "unknown network phase build",
            ),
            (network(json!({ "runtime": [] })), "invalid type"),
            (
                network(json!({ "runtime": { "block": [] } })),
                "unknown runtime decision block",
            ),
            (
                network(json!({ "install": { "deny": ["a"] } })),
                "install.deny is not forged",
            ),
            (
                network(json!({ "runtime": { "allow": "a" } })),
                "invalid type",
            ),
            (
                network(json!({ "runtime": { "allow": [""] } })),
                "runtime.allow: invalid host pattern \"\": empty",
            ),
            (
                network(json!({ "runtime": { "deny": ["https://tracker.example.com"] } })),
                "runtime.deny: invalid host pattern \"https://tracker.example.com\": a host, not a URL",
            ),
            (
                network(json!({ "install": { "allow": ["pypi.org/simple"] } })),
                "install.allow: invalid host pattern \"pypi.org/simple\": a path",
            ),
        ]);
    }

    #[test]
    fn a_step_can_be_given_the_mcp_gateway_of_the_sandbox() {
        let step = LifecycleStep::new("echo $MCP_GATEWAY_URL").with_gateway_env();
        assert_eq!(step.env, ["MCP_GATEWAY_URL", "MCP_SENTINEL_TOKEN_NAME"]);
    }

    #[test]
    fn what_can_be_forged_is_accepted() {
        let value = with(
            &json!({ "type": LIFECYCLE, "name": "tools", "description": "d", "optional": false, "config": {
            "install": [{ "command": ["sh", "-c", "x"], "user": "0", "description": "d", "env": ["_A1"], "background": false }],
            "startup": [{ "command": "y", "background": true }] } }),
        );
        let descriptor = KitDescriptor::from_json(value.clone()).unwrap();
        assert_eq!(
            descriptor.to_json(),
            value,
            "read and written back as it was"
        );
        let network = json!({ "type": NETWORK, "name": "n", "description": "d", "optional": true,
            "config": { "install": { "allow": ["a:80"] } } });
        KitDescriptor::from_json(with(&network)).unwrap();
        let metadata = json!({ "schemaVersion": "3", "kind": "mixin", "author": { "name": "Acme" },
            "licenses": ["MIT"], "sourceUrl": "https://example.com/acme", "capabilities": [] });
        assert_eq!(
            KitDescriptor::from_json(metadata.clone())
                .unwrap()
                .to_json(),
            metadata
        );
    }

    #[test]
    fn lists_grow_one_item_at_a_time() {
        let descriptor = KitDescriptor::mixin("Acme")
            .with_install(LifecycleStep::new("one"))
            .with_network_allow(["a.example.com", "b.example.com"])
            .with_install(LifecycleStep::new("two"))
            .with_network_allow(["c.example.com"]);
        let value = descriptor.to_json();
        assert_eq!(
            value["capabilities"][0]["config"]["install"][1]["command"],
            "two"
        );
        assert_eq!(
            value["capabilities"][1]["config"]["runtime"]["allow"],
            json!(["a.example.com", "b.example.com", "c.example.com"])
        );
        assert!(value.get("description").is_none());
    }

    #[test]
    fn files_are_read_as_their_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spec.yaml");
        let descriptor = KitDescriptor::mixin("Acme").with_description("Acme tools");
        std::fs::write(&path, descriptor.to_spec()).unwrap();
        assert_eq!(KitDescriptor::read(&path).unwrap(), descriptor);
    }

    #[test]
    fn unreadable_files_are_file_errors() {
        let error = KitDescriptor::read("/zz-docker-sandbox-none/spec.yaml").unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("cannot access /zz-docker-sandbox-none/spec.yaml"),
            "{error}"
        );
    }

    #[test]
    fn published_steps_keep_the_fields_this_crate_does_not_know() {
        let config: LifecycleConfig = serde_json::from_value(json!({
            "install": [{ "command": "x", "when": "always" }], "build": [] }))
        .unwrap();
        assert_eq!(config.install[0].extra["when"], "always");
        assert_eq!(to_json(&config)["build"], json!([]));
        let rules: NetworkConfig =
            serde_json::from_value(json!({ "runtime": { "allow": ["a"], "audit": ["b"] } }))
                .unwrap();
        assert!(!rules.runtime.is_empty() && rules.install.is_empty());
        assert_eq!(to_json(&rules)["runtime"]["audit"], json!(["b"]));
    }
}
