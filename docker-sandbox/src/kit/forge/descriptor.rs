//! Kit descriptors: what a `spec.yaml` holds, read from JSON or built in Rust.
//!
//! The daemon reads the v3 descriptor of every kit it composes and derives the
//! permissions, network rules and commands of the sandbox from it. A forged
//! mixin may declare what the audit measured the daemon accepting from one:
//! `lifecycle@1` install and startup commands, and `network-policy@1` rules.
//! Anything else is refused rather than sent on, so that a forged kit never
//! half succeeds in silence.
//!
//! Descriptors are JSON. A `spec.yaml` written in the JSON subset of YAML,
//! after its `# syntax=docker/sandbox-kit:3` line, is read as it is, and the
//! `sbx` CLI reads it too.

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};

/// First line of a v3 descriptor: the `BuildKit` frontend that reads it.
const SYNTAX_LINE: &str = "# syntax=docker/sandbox-kit:3";

/// Capability of the install and startup commands.
pub(super) const LIFECYCLE: &str = "com.docker.sandbox/lifecycle@1";

/// Capability of the network rules.
pub(super) const NETWORK: &str = "com.docker.sandbox/network-policy@1";

/// Top-level fields a forged mixin may carry.
const FIELDS: &[&str] = &[
    "schemaVersion",
    "kind",
    "displayName",
    "description",
    "version",
    "author",
    "licenses",
    "sourceUrl",
    "capabilities",
];

/// Fields of a capability entry.
const CAPABILITY_FIELDS: &[&str] = &["type", "config", "optional", "name", "description"];

/// Fields of a lifecycle step.
const STEP_FIELDS: &[&str] = &["command", "user", "description", "env", "background"];

/// A v3 kit descriptor (feature `kit-forge`), read from JSON or built in Rust.
///
/// ```
/// use docker_sandbox::kit::{KitDescriptor, LifecycleStep};
///
/// let descriptor = KitDescriptor::mixin("Acme tools")
///     .with_description("Tools for the Acme API")
///     .with_network_allow(["api.acme.example:443"])
///     .with_install(LifecycleStep::new("pip install acme-cli").as_user("agent"));
/// let spec = descriptor.to_spec();
/// assert!(spec.starts_with("# syntax=docker/sandbox-kit:3\n"));
/// assert_eq!(KitDescriptor::parse(&spec)?, descriptor);
/// # Ok::<(), docker_sandbox::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct KitDescriptor {
    /// The descriptor, as JSON.
    value: Value,
}

impl KitDescriptor {
    /// An empty mixin named `display_name`, to build on with the `with_*`
    /// methods.
    pub fn mixin(display_name: impl Into<String>) -> Self {
        Self {
            value: json!({
                "schemaVersion": "3",
                "kind": "mixin",
                "displayName": display_name.into(),
                "capabilities": [],
            }),
        }
    }

    /// A descriptor from its JSON.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidKit`] when it is not a v3 mixin this crate can
    /// forge.
    pub fn from_json(value: Value) -> Result<Self> {
        let descriptor = Self { value };
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
    /// Returns [`Error::File`] when the file cannot be read, and
    /// [`Error::InvalidKit`] as [`KitDescriptor::parse`] does.
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| Error::File {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Sets the description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.value["description"] = json!(description.into());
        self
    }

    /// Sets the version of the kit, e.g. `1.0.0`.
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.value["version"] = json!(version.into());
        self
    }

    /// Adds a command run once, when the sandbox is created, before its agent
    /// starts.
    #[must_use]
    pub fn with_install(mut self, step: LifecycleStep) -> Self {
        self.push(LIFECYCLE, "/config/install", step.into_json());
        self
    }

    /// Adds a command run at every start of the sandbox, alongside its agent.
    #[must_use]
    pub fn with_startup(mut self, step: LifecycleStep) -> Self {
        self.push(LIFECYCLE, "/config/startup", step.into_json());
        self
    }

    /// Allows `hosts` (`host` or `host:port`) while the sandbox runs.
    #[must_use]
    pub fn with_network_allow(
        mut self,
        hosts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        for host in hosts {
            self.push(NETWORK, "/config/runtime/allow", json!(host.into()));
        }
        self
    }

    /// Denies `hosts` (`host` or `host:port`) while the sandbox runs.
    #[must_use]
    pub fn with_network_deny(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        for host in hosts {
            self.push(NETWORK, "/config/runtime/deny", json!(host.into()));
        }
        self
    }

    /// The display name, if the descriptor has one.
    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        self.value.get("displayName").and_then(Value::as_str)
    }

    /// The descriptor as JSON.
    #[must_use]
    pub fn as_json(&self) -> &Value {
        &self.value
    }

    /// The text of a `spec.yaml` holding the descriptor: its `# syntax=` line,
    /// then the descriptor as JSON, which the `sbx` CLI reads as YAML.
    #[must_use]
    pub fn to_spec(&self) -> String {
        // Serializing a `Value` cannot fail: its map keys are strings.
        let json = serde_json::to_string_pretty(&self.value).unwrap_or_default();
        format!("{SYNTAX_LINE}\n{json}\n")
    }

    /// Appends `item` to the list at `pointer` in the capability `kind`,
    /// creating the capability and the list when missing.
    fn push(&mut self, kind: &str, pointer: &str, item: Value) {
        let mut capabilities: Vec<Value> =
            serde_json::from_value(self.value["capabilities"].take()).unwrap_or_default();
        let found = capabilities
            .iter()
            .position(|c| c.get("type").and_then(Value::as_str) == Some(kind));
        let index = found.unwrap_or_else(|| {
            capabilities.push(json!({ "type": kind, "config": {} }));
            capabilities.len() - 1
        });
        // A validated descriptor holds objects along the way: indexing a
        // missing key inserts it.
        let mut target = &mut capabilities[index];
        for key in pointer.split('/').skip(1) {
            target = &mut target[key];
        }
        match target {
            Value::Array(items) => items.push(item),
            other => *other = json!([item]),
        }
        self.value["capabilities"] = Value::Array(capabilities);
    }

    /// Checks that the descriptor is a v3 mixin this crate can forge.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidKit`], naming the kit and what is wrong.
    pub(super) fn validate(&self) -> Result<()> {
        check(&self.value).map_err(|message| Error::InvalidKit {
            kit: self.display_name().unwrap_or("descriptor").to_owned(),
            message,
        })
    }
}

/// A command a mixin runs when the sandbox is created (install) or at each of
/// its starts (startup).
///
/// ```
/// use docker_sandbox::kit::LifecycleStep;
///
/// let step = LifecycleStep::new("echo ready > /tmp/ready")
///     .as_user("agent")
///     .with_description("Marks the sandbox ready")
///     .with_env(["MCP_GATEWAY_URL"]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleStep {
    /// Shell command, run with `sh -c`.
    command: String,
    /// User it runs as, e.g. `agent`, or `0` for root.
    user: Option<String>,
    /// What it does, shown as it runs.
    description: Option<String>,
    /// Variables of the sandbox it receives: install commands run in an
    /// emptied environment otherwise.
    env: Vec<String>,
    /// Whether a startup command runs in the background.
    background: bool,
}

impl LifecycleStep {
    /// The shell command `command`, run with `sh -c`.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            user: None,
            description: None,
            env: Vec::new(),
            background: false,
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

    /// Runs a startup command in the background.
    #[must_use]
    pub fn in_background(mut self) -> Self {
        self.background = true;
        self
    }

    /// The step as the descriptor holds it.
    fn into_json(self) -> Value {
        let mut step = Map::new();
        step.insert("command".into(), json!(self.command));
        if let Some(user) = self.user {
            step.insert("user".into(), json!(user));
        }
        if let Some(description) = self.description {
            step.insert("description".into(), json!(description));
        }
        if !self.env.is_empty() {
            step.insert("env".into(), json!(self.env));
        }
        if self.background {
            step.insert("background".into(), json!(true));
        }
        Value::Object(step)
    }
}

/// What is wrong with `descriptor` as a mixin to forge, if anything.
fn check(descriptor: &Value) -> std::result::Result<(), String> {
    let fields = descriptor
        .as_object()
        .ok_or("a descriptor is a JSON object")?;
    if fields.get("schemaVersion").and_then(Value::as_str) != Some("3") {
        return Err("only v3 descriptors (schemaVersion \"3\") are forged".to_owned());
    }
    if fields.get("kind").and_then(Value::as_str) != Some("mixin") {
        return Err("only mixins are forged; a base kit comes from a package".to_owned());
    }
    if let Some(field) = fields.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(format!(
            "the field {field} is not one a forged mixin may carry"
        ));
    }
    if descriptor.to_string().contains("${{") {
        return Err("kit arguments are not supported in forged mixins".to_owned());
    }
    let capabilities = match fields.get("capabilities") {
        None => return Ok(()),
        Some(Value::Array(capabilities)) => capabilities,
        Some(_) => return Err("capabilities is a list".to_owned()),
    };
    for (index, capability) in capabilities.iter().enumerate() {
        check_capability(capability).map_err(|e| format!("capabilities[{index}]: {e}"))?;
    }
    Ok(())
}

/// What is wrong with one capability entry, if anything.
fn check_capability(capability: &Value) -> std::result::Result<(), String> {
    let fields = capability.as_object().ok_or("an object")?;
    if let Some(field) = fields
        .keys()
        .find(|k| !CAPABILITY_FIELDS.contains(&k.as_str()))
    {
        return Err(format!("unknown field {field}"));
    }
    let config = fields.get("config").cloned().unwrap_or_else(|| json!({}));
    let config = config.as_object().ok_or("config is an object")?;
    match fields.get("type").and_then(Value::as_str) {
        Some(LIFECYCLE) => check_lifecycle(config),
        Some(NETWORK) => check_network(config),
        Some(other) => Err(format!(
            "{other} is not forged: only {LIFECYCLE} and {NETWORK} are"
        )),
        None => Err("no type".to_owned()),
    }
}

/// What is wrong with a `lifecycle@1` configuration, if anything: every step
/// has a command, so that none is dropped in silence by the daemon.
fn check_lifecycle(config: &Map<String, Value>) -> std::result::Result<(), String> {
    for (phase, steps) in config {
        if phase != "install" && phase != "startup" {
            return Err(format!("unknown lifecycle phase {phase}"));
        }
        let steps = steps
            .as_array()
            .ok_or_else(|| format!("{phase} is a list"))?;
        for (index, step) in steps.iter().enumerate() {
            check_step(phase, step).map_err(|e| format!("{phase}[{index}]: {e}"))?;
        }
    }
    Ok(())
}

/// What is wrong with one lifecycle step of `phase`, if anything.
fn check_step(phase: &str, step: &Value) -> std::result::Result<(), String> {
    let fields = step.as_object().ok_or("an object")?;
    if let Some(field) = fields.keys().find(|k| !STEP_FIELDS.contains(&k.as_str())) {
        return Err(format!("unknown field {field}"));
    }
    let command_ok = match fields.get("command") {
        Some(Value::String(command)) => !command.trim().is_empty(),
        Some(Value::Array(words)) => !words.is_empty() && words.iter().all(Value::is_string),
        _ => false,
    };
    if !command_ok {
        return Err("no command: the daemon would skip the step without a word".to_owned());
    }
    for key in ["user", "description"] {
        if fields.get(key).is_some_and(|v| !v.is_string()) {
            return Err(format!("{key} is a string"));
        }
    }
    let env = fields.get("env").cloned().unwrap_or_else(|| json!([]));
    let names = env.as_array().ok_or("env is a list")?;
    if let Some(bad) = names
        .iter()
        .find(|n| !n.as_str().is_some_and(is_variable_name))
    {
        return Err(format!("{bad} is not a variable name"));
    }
    match fields.get("background") {
        None | Some(Value::Bool(false)) => Ok(()),
        Some(Value::Bool(true)) if phase == "startup" => Ok(()),
        Some(Value::Bool(true)) => Err("only startup commands run in the background".to_owned()),
        Some(_) => Err("background is a boolean".to_owned()),
    }
}

/// What is wrong with a `network-policy@1` configuration, if anything: rules
/// at run time, and allowed hosts during installation.
fn check_network(config: &Map<String, Value>) -> std::result::Result<(), String> {
    for (phase, rules) in config {
        let decisions: &[&str] = match phase.as_str() {
            "runtime" => &["allow", "deny"],
            "install" => &["allow"],
            _ => return Err(format!("unknown network phase {phase}")),
        };
        let rules = rules
            .as_object()
            .ok_or_else(|| format!("{phase} is an object"))?;
        for (decision, hosts) in rules {
            if !decisions.contains(&decision.as_str()) {
                return Err(format!("{phase}.{decision} is not forged"));
            }
            let hosts = hosts
                .as_array()
                .ok_or_else(|| format!("{phase}.{decision} is a list"))?;
            if hosts
                .iter()
                .any(|h| h.as_str().is_none_or(|h| h.trim().is_empty()))
            {
                return Err(format!("{phase}.{decision} holds an empty host"));
            }
        }
    }
    Ok(())
}

/// Whether `name` is the name of an environment variable.
fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message of the `InvalidKit` error `value` gives.
    fn refusal(value: Value) -> String {
        let error = KitDescriptor::from_json(value).unwrap_err();
        assert!(matches!(error, Error::InvalidKit { .. }), "{error}");
        error.to_string()
    }

    /// A mixin whose only capability is `capability`.
    fn with(capability: &Value) -> Value {
        json!({ "schemaVersion": "3", "kind": "mixin", "displayName": "Test", "capabilities": [capability] })
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
        let value = descriptor.as_json();
        assert_eq!(value["capabilities"].as_array().unwrap().len(), 2);
        assert_eq!(
            value["capabilities"][0]["config"]["runtime"],
            json!({ "allow": ["a.example.com:443"], "deny": ["b.example.com"] })
        );
        assert_eq!(
            value["capabilities"][1]["config"]["startup"][0],
            json!({ "command": "date", "background": true })
        );
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
            descriptor.as_json()["capabilities"][0]["config"]["install"][0],
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

    /// Checks that each descriptor of `cases` is refused with a message
    /// holding its expected words.
    fn assert_refused(cases: Vec<(Value, &str)>) {
        for (value, expected) in cases {
            let message = refusal(value.clone());
            assert!(message.contains(expected), "{value}: {message}");
        }
    }

    #[test]
    fn what_cannot_be_forged_is_refused() {
        assert_refused(vec![
            (json!([]), "a descriptor is a JSON object"),
            (json!({ "schemaVersion": "2", "kind": "mixin" }), "only v3"),
            (
                json!({ "schemaVersion": "3", "kind": "workload" }),
                "only mixins",
            ),
            (
                json!({ "schemaVersion": "3", "kind": "mixin", "extends": "x" }),
                "extends",
            ),
            (
                json!({ "schemaVersion": "3", "kind": "mixin", "capabilities": {} }),
                "is a list",
            ),
            (with(&json!("lifecycle")), "an object"),
            (
                with(&json!({ "type": LIFECYCLE, "extra": 1 })),
                "unknown field extra",
            ),
            (
                with(&json!({ "type": LIFECYCLE, "config": [] })),
                "config is an object",
            ),
            (with(&json!({ "config": {} })), "no type"),
            (
                with(&json!({ "type": "com.docker.sandbox/credential@1" })),
                "is not forged",
            ),
            (
                with(
                    &json!({ "type": LIFECYCLE, "config": { "install": [{ "command": "echo ${{ kit.args.x }}" }] } }),
                ),
                "kit arguments",
            ),
        ]);
    }

    #[test]
    fn lifecycle_steps_that_cannot_be_forged_are_refused() {
        let step = |phase: &str, step: Value| {
            with(&json!({ "type": LIFECYCLE, "config": { phase: [step] } }))
        };
        assert_refused(vec![
            (
                with(&json!({ "type": LIFECYCLE, "config": { "teardown": [] } })),
                "unknown lifecycle phase",
            ),
            (
                with(&json!({ "type": LIFECYCLE, "config": { "install": {} } })),
                "install is a list",
            ),
            (step("install", json!(1)), "an object"),
            (
                step("install", json!({ "command": "x", "when": 1 })),
                "unknown field when",
            ),
            (step("install", json!({ "user": "agent" })), "no command"),
            (step("install", json!({ "command": " " })), "no command"),
            (step("install", json!({ "command": [] })), "no command"),
            (
                step("install", json!({ "command": "x", "user": 0 })),
                "user is a string",
            ),
            (
                step("install", json!({ "command": "x", "env": "A" })),
                "env is a list",
            ),
            (
                step("install", json!({ "command": "x", "env": ["1A"] })),
                "not a variable name",
            ),
            (
                step("install", json!({ "command": "x", "background": true })),
                "only startup",
            ),
            (
                step("startup", json!({ "command": "x", "background": "yes" })),
                "background is a boolean",
            ),
        ]);
    }

    #[test]
    fn network_rules_that_cannot_be_forged_are_refused() {
        let network = |config: Value| with(&json!({ "type": NETWORK, "config": config }));
        assert_refused(vec![
            (network(json!({ "build": {} })), "unknown network phase"),
            (network(json!({ "runtime": [] })), "runtime is an object"),
            (
                network(json!({ "install": { "deny": [] } })),
                "install.deny is not forged",
            ),
            (
                network(json!({ "runtime": { "allow": "a" } })),
                "runtime.allow is a list",
            ),
            (
                network(json!({ "runtime": { "allow": [""] } })),
                "empty host",
            ),
        ]);
    }

    #[test]
    fn what_can_be_forged_is_accepted() {
        let value = with(&json!({ "type": LIFECYCLE, "optional": false, "config": {
            "install": [{ "command": ["sh", "-c", "x"], "user": "0", "description": "d", "env": ["_A1"], "background": false }],
            "startup": [{ "command": "y", "background": true }] } }));
        KitDescriptor::from_json(value).unwrap();
        KitDescriptor::from_json(with(
            &json!({ "type": NETWORK, "config": { "install": { "allow": ["a:80"] } } }),
        ))
        .unwrap();
        KitDescriptor::from_json(json!({ "schemaVersion": "3", "kind": "mixin" })).unwrap();
    }

    #[test]
    fn lists_grow_one_item_at_a_time() {
        let descriptor = KitDescriptor::mixin("Acme")
            .with_network_allow(["a.example.com", "b.example.com"])
            .with_install(LifecycleStep::new("one"))
            .with_install(LifecycleStep::new("two"));
        let value = descriptor.as_json();
        assert_eq!(
            value["capabilities"][0]["config"]["runtime"]["allow"],
            json!(["a.example.com", "b.example.com"])
        );
        assert_eq!(
            value["capabilities"][1]["config"]["install"][1]["command"],
            "two"
        );
        assert_eq!(value["description"], Value::Null);
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
        assert!(matches!(
            KitDescriptor::read("/zz-docker-sandbox-none/spec.yaml"),
            Err(Error::File { .. })
        ));
    }
}
