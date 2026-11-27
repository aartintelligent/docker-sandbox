//! Kit artifacts: the entries of `kit_artifacts` the daemon composes a sandbox
//! from, one per kit.
//!
//! The fields are the ones the `sbx` CLI writes, as the audit captured them,
//! typed here so that every field name is checked when the crate is built.
//! The daemon needs `manifest` (its `template` names the image of the kit),
//! `v3`, from which it derives permissions and network rules, `v3Effective`
//! for a base kit, and `commands`, the commands it runs: without them a
//! sandbox is created and nothing is installed, without any error. The other
//! fields are written for fidelity.
//!
//! The descriptors an artifact carries (`v3`, `v3Effective`, the capabilities
//! of `v3Selection`) are JSON: those of published kits are forwarded as they
//! are, with capabilities this crate does not know.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::descriptor::{
    Command, KitDescriptor, LIFECYCLE, LifecycleConfig, LifecycleStep, NetworkConfig, SchemaVersion,
};
use super::to_json;

/// The capability types the CLI resolves, in the order it writes them in
/// `v3Effective`, whatever their order in the descriptor; capabilities of one
/// type keep theirs. Measured by the audit on the kits `shell`, `claude`,
/// `copilot` and `opencode` and on mixins: the daemon withholds the
/// credentials of a published kit whose effective descriptor comes in another
/// order.
pub(super) const RESOLVED: &[&str] = &[
    "com.docker.sandbox/network-policy@1",
    "com.docker.sandbox/sbx@1",
    "com.docker.sandbox/credential@1",
    "com.docker.sandbox/agent-skills@1",
    "com.docker.sandbox/acp@1",
    "com.docker.sandbox/agent-sessions@1",
    "com.docker.sandbox/agent-interactive-sessions@1",
    "com.docker.sandbox/lifecycle@1",
    "com.docker.sandbox/agent-context@1",
    "com.docker.sandbox/agent-context@2",
];

/// The capability types the CLI leaves out of a kit as unsupported, when the
/// kit declares them optional.
pub(super) const UNSUPPORTED: &[&str] = &["com.docker.sandbox/dockerd@1"];

/// Variables every install command keeps from the environment of the
/// sandbox, in the order the CLI writes them.
const INSTALL_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "TERM",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
];

/// The artifact of one kit.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct KitArtifact {
    /// Identity of the kit and the image it runs from.
    pub(super) manifest: Manifest,
    /// The descriptor, as the kit declares it.
    pub(super) v3: Value,
    /// The descriptor of the selected capabilities, normalized.
    pub(super) v3_effective: Value,
    /// Which capabilities are selected, and why.
    pub(super) v3_selection: Selection,
    /// Digest of the image of the kit.
    pub(super) v3_digest: String,
    /// Reference of the kit, as `kits` lists it.
    pub(super) v3_reference: String,
    /// Entrypoint of the image of a base kit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) v3_entrypoint: Option<Vec<String>>,
    /// Where a published kit is stored, `oci://<reference>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) storage_source: Option<String>,
    /// Where the kit comes from.
    pub(super) kit_usage: KitUsage,
    /// The Docker Hub repository of a kit published there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) kit_hub_publication: Option<HubPublication>,
    /// Licenses of a base kit, as it declares them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) licenses: Option<Value>,
    /// Content of the agent context file of a base kit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) agent_context: Option<String>,
    /// Commands the daemon runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) commands: Option<Commands>,
    /// Network rules while the sandbox runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) caps: Option<Caps>,
    /// Credentials the kit declares.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) credentials: Vec<Credential>,
    /// Services of the credentials the kit works without.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) optional_credentials: Vec<String>,
    /// Skills directories the kit asks for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) skills_requests: Vec<SkillsRequest>,
    /// Declarations of the arguments of a base kit, without their variables.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub(super) args: Map<String, Value>,
    /// Values of those arguments.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) v3_arg_values: BTreeMap<String, String>,
    /// Variables the arguments set.
    #[serde(skip_serializing_if = "Environment::is_empty")]
    pub(super) environment: Environment,
}

/// The `manifest` of an artifact.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Manifest {
    /// What the kit is for.
    pub(super) description: Option<String>,
    /// Name shown for the kit.
    pub(super) display_name: Option<String>,
    /// Kind of kit.
    pub(super) kind: ManifestKind,
    /// Name of the kit, e.g. `shell`.
    pub(super) name: String,
    /// Version of the descriptor format.
    pub(super) schema_version: SchemaVersion,
    /// Image the kit runs from: a published image pinned by digest, or the
    /// source image of a mixin.
    pub(super) template: String,
    /// The fields of the manifest of a base kit.
    #[serde(flatten)]
    pub(super) base: Option<BaseManifest>,
}

/// The fields only the manifest of a base kit has.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BaseManifest {
    /// Where the kit is defined, `null` when it does not say.
    #[serde(rename = "sourceURL")]
    pub(super) source_url: Option<String>,
    /// Program of its agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) binary: Option<String>,
    /// Options the program starts with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) run_options: Option<Vec<String>>,
    /// File its agent context goes to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ai_filename: Option<String>,
}

/// The kind of kit a manifest describes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ManifestKind {
    /// A mixin.
    #[default]
    Mixin,
    /// A base kit.
    Sandbox,
}

/// Where a kit comes from; a local directory by default.
#[derive(Debug, Serialize)]
pub(super) struct KitUsage {
    /// Identifier of the kit, which the CLI leaves unknown.
    kit_identifier: &'static str,
    /// Kind of source.
    source_type: SourceType,
}

/// The kind of source of a kit.
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
enum SourceType {
    /// A local directory.
    #[default]
    LocalDir,
    /// A published image.
    Hub,
}

/// The Docker Hub repository of a published kit.
#[derive(Debug, Serialize)]
pub(super) struct HubPublication {
    /// Digest of its image.
    pub(super) digest: String,
    /// Repository, e.g. `sbx/shell`.
    pub(super) repository: String,
}

/// The capability selection of a kit.
#[derive(Debug, Default, Serialize)]
pub(super) struct Selection {
    /// The selected capabilities, each with its source.
    capabilities: Vec<SelectedCapability>,
    /// Entries of the selected capabilities.
    selected: Vec<SelectionEntry>,
    /// Entries of the capabilities left out.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<SelectionEntry>,
}

/// A selected capability: its JSON, and where it comes from.
#[derive(Debug, Serialize)]
struct SelectedCapability {
    /// The capability, as the kit declares it.
    #[serde(flatten)]
    capability: Map<String, Value>,
    /// Where it comes from.
    source: Source,
}

/// Why a capability is selected or left out.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SelectionEntry {
    /// The decision.
    decisions: Vec<Decision>,
    /// Where its members come from.
    member_sources: Vec<Source>,
    /// Its members: its path alone.
    members: Vec<String>,
    /// Name of the capability, if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<Value>,
    /// Its path in the descriptor, e.g. `capabilities[0]`.
    path: String,
    /// The paths left out, for a capability left out.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rejected: Vec<String>,
    /// Where it comes from.
    source: Source,
}

/// A capability of a kit: the kit, and the path of the capability in it.
#[derive(Debug, Clone, Serialize)]
struct Source {
    /// Reference of the kit.
    kit: String,
    /// Path of the capability, e.g. `capabilities[0]`.
    path: String,
}

/// The decision taken on a capability.
#[derive(Debug, Serialize)]
struct Decision {
    /// Whether it is selected.
    accepted: bool,
    /// Why it is left out.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// The commands the daemon runs, by phase.
#[derive(Debug, Default, Serialize)]
pub(super) struct Commands {
    /// Run once, when the sandbox is created.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    install: Vec<CommandEntry>,
    /// Run at every start of the sandbox.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    startup: Vec<CommandEntry>,
}

/// One command the daemon runs.
#[derive(Debug, Serialize)]
struct CommandEntry {
    /// The command, ready to run.
    command: Command,
    /// What it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// User it runs as.
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    /// Whether it runs in the background.
    #[serde(skip_serializing_if = "Option::is_none")]
    background: Option<bool>,
}

/// The capabilities the daemon applies itself: network rules.
#[derive(Debug, Serialize)]
pub(super) struct Caps {
    /// Network rules while the sandbox runs.
    network: NetworkCaps,
}

/// Hosts allowed and denied while the sandbox runs.
#[derive(Debug, Default, Serialize)]
struct NetworkCaps {
    /// Hosts allowed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    allow: Vec<String>,
    /// Hosts denied.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deny: Vec<String>,
}

/// A credential a kit declares: what the proxy injects, never a value.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Credential {
    /// Service of the credential.
    pub(super) service: Option<String>,
    /// What it gives access to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    /// How an API key is injected, named `""` when the kit names it not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) api_key: Option<Value>,
    /// How an OAuth token is obtained.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) oauth: Option<Value>,
}

/// A skills directory a kit asks for.
#[derive(Debug, Serialize)]
pub(super) struct SkillsRequest {
    /// How the shared skills store is mounted.
    pub(super) mode: &'static str,
    /// Whether the kit works without it.
    pub(super) optional: bool,
    /// Directory of the skills inside the sandbox.
    pub(super) path: Option<String>,
}

/// The variables the arguments of a kit set.
#[derive(Debug, Default, Serialize)]
pub(super) struct Environment {
    /// Values, by variable.
    pub(super) variables: BTreeMap<String, String>,
}

/// What the artifact of a mixin is built from.
pub(super) struct MixinParts<'a> {
    /// Name of the mixin, the last part of its reference.
    pub(super) name: &'a str,
    /// Reference the mixin is listed under in `kits`.
    pub(super) reference: &'a str,
    /// The validated descriptor.
    pub(super) descriptor: &'a KitDescriptor,
    /// Name of its source image, e.g. `sbx-kit-src:spec-…`.
    pub(super) image: &'a str,
    /// Digest of the manifest of its source image.
    pub(super) manifest_digest: &'a str,
}

impl KitUsage {
    /// A kit from a local directory.
    pub(super) fn local() -> Self {
        Self {
            kit_identifier: "unknown",
            source_type: SourceType::LocalDir,
        }
    }

    /// A published kit.
    pub(super) fn published() -> Self {
        Self {
            kit_identifier: "unknown",
            source_type: SourceType::Hub,
        }
    }
}

impl Default for KitUsage {
    fn default() -> Self {
        Self::local()
    }
}

impl Environment {
    /// Whether no variable is set.
    fn is_empty(&self) -> bool {
        self.variables.is_empty()
    }
}

/// The artifact of a mixin; it equals the one of the CLI but for the name and
/// digest of the source image.
pub(super) fn mixin(parts: &MixinParts<'_>) -> KitArtifact {
    let spec = parts.descriptor.spec();
    let v3 = parts.descriptor.to_json();
    let listed: Vec<Value> = capabilities(&v3).cloned().collect();
    KitArtifact {
        manifest: Manifest {
            description: spec.description.clone(),
            display_name: spec.display_name.clone(),
            kind: ManifestKind::Mixin,
            name: parts.name.to_owned(),
            template: parts.image.to_owned(),
            ..Manifest::default()
        },
        v3_effective: effective(&v3),
        v3_selection: selection(&listed, parts.reference, |_| None),
        v3_digest: parts.manifest_digest.to_owned(),
        v3_reference: parts.reference.to_owned(),
        kit_usage: KitUsage::local(),
        commands: commands(spec.lifecycles()),
        caps: caps(spec.network_policies()),
        v3,
        ..KitArtifact::default()
    }
}

/// The capabilities of the descriptor `descriptor`, in order.
pub(super) fn capabilities(descriptor: &Value) -> impl Iterator<Item = &Value> {
    descriptor
        .get("capabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// Whether `capability` is of the type `kind`.
pub(super) fn is(capability: &Value, kind: &str) -> bool {
    capability.get("type").and_then(Value::as_str) == Some(kind)
}

/// `descriptor` normalized as the CLI does: its capabilities in the order of
/// [`RESOLVED`], lifecycle commands written as `["sh", "-c", command]`. The
/// descriptor is JSON, so that the fields this crate does not know are kept.
pub(super) fn effective(descriptor: &Value) -> Value {
    let mut effective = descriptor.clone();
    let Some(capabilities) = effective
        .get_mut("capabilities")
        .and_then(Value::as_array_mut)
    else {
        return effective;
    };
    // A stable sort: capabilities of one type keep their order.
    capabilities.sort_by_key(rank);
    for capability in capabilities.iter_mut().filter(|c| is(c, LIFECYCLE)) {
        for phase in ["install", "startup"] {
            let steps = capability
                .pointer_mut(&format!("/config/{phase}"))
                .and_then(Value::as_array_mut);
            for step in steps.into_iter().flatten() {
                if let Some(command) = step.get("command").and_then(Value::as_str) {
                    step["command"] = json!(["sh", "-c", command]);
                }
            }
        }
    }
    effective
}

/// The place of `capability` in [`RESOLVED`]; after them all for a type the
/// CLI does not resolve.
fn rank(capability: &Value) -> usize {
    let kind = capability
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    RESOLVED
        .iter()
        .position(|resolved| *resolved == kind)
        .unwrap_or(RESOLVED.len())
}

/// The selection of the capabilities `listed` of the kit `kit`: each is
/// selected, unless `rejection` gives the reason it is left out.
pub(super) fn selection(
    listed: &[Value],
    kit: &str,
    rejection: impl Fn(&Value) -> Option<String>,
) -> Selection {
    let mut selection = Selection::default();
    for (index, capability) in listed.iter().enumerate() {
        let path = format!("capabilities[{index}]");
        let source = Source {
            kit: kit.to_owned(),
            path: path.clone(),
        };
        let rejected = rejection(capability);
        let entry = SelectionEntry {
            decisions: vec![Decision {
                accepted: rejected.is_none(),
                message: rejected.clone(),
            }],
            member_sources: vec![source.clone()],
            members: vec![path.clone()],
            name: capability.get("name").cloned(),
            rejected: rejected.iter().map(|_| path.clone()).collect(),
            path,
            source: source.clone(),
        };
        if rejected.is_some() {
            selection.skipped.push(entry);
        } else {
            selection.capabilities.push(SelectedCapability {
                capability: capability.as_object().cloned().unwrap_or_default(),
                source,
            });
            selection.selected.push(entry);
        }
    }
    selection
}

/// The commands of the lifecycle configurations `lifecycles`: install
/// commands wrapped in an emptied environment that keeps [`INSTALL_ENV`] and
/// the variables the step names, startup commands as `["sh", "-c", command]`;
/// a command written as a list is kept as it is. `None` without any.
pub(super) fn commands<'a>(
    lifecycles: impl IntoIterator<Item = &'a LifecycleConfig>,
) -> Option<Commands> {
    let mut commands = Commands::default();
    for config in lifecycles {
        for step in &config.install {
            let command = match &step.command {
                Command::Shell(command) => Command::Shell(wrap_install(command, &step.env)),
                list @ Command::Exec(_) => list.clone(),
            };
            commands.install.push(CommandEntry::of(step, command));
        }
        for step in &config.startup {
            let command = match &step.command {
                Command::Shell(command) => {
                    Command::Exec(vec!["sh".into(), "-c".into(), command.clone()])
                }
                list @ Command::Exec(_) => list.clone(),
            };
            commands.startup.push(CommandEntry::of(step, command));
        }
    }
    let empty = commands.install.is_empty() && commands.startup.is_empty();
    (!empty).then_some(commands)
}

impl CommandEntry {
    /// The entry of `step`, run as `command`.
    fn of(step: &LifecycleStep, command: Command) -> Self {
        Self {
            command,
            description: step.description.clone(),
            user: step.user.clone(),
            background: step.background,
        }
    }
}

/// `env -i NAME="$NAME" … sh -c '<command>'`, the form the CLI gives install
/// commands, keeping the variables `named` too.
fn wrap_install(command: &str, named: &[String]) -> String {
    let mut wrapped = String::from("env -i");
    for name in INSTALL_ENV
        .iter()
        .copied()
        .chain(named.iter().map(String::as_str))
    {
        let _ = write!(wrapped, " {name}=\"${name}\"");
    }
    let _ = write!(wrapped, " sh -c '{}'", command.replace('\'', r"'\''"));
    wrapped
}

/// The run-time network rules of the network configurations `policies`, as
/// the `caps` of the artifact. `None` without any.
pub(super) fn caps<'a>(policies: impl IntoIterator<Item = &'a NetworkConfig>) -> Option<Caps> {
    let mut network = NetworkCaps::default();
    for config in policies {
        network.allow.extend(config.runtime.allow.iter().cloned());
        network.deny.extend(config.runtime.deny.iter().cloned());
    }
    let empty = network.allow.is_empty() && network.deny.is_empty();
    (!empty).then_some(Caps { network })
}

impl KitArtifact {
    /// The artifact as JSON, as `kit_artifacts` holds it.
    pub(super) fn into_json(self) -> Value {
        to_json(&self)
    }
}

#[cfg(test)]
mod tests {
    use super::super::descriptor::NETWORK;
    use super::*;

    /// A mixin with network rules, an install command naming a variable, a
    /// startup command in the background and one written as a list.
    fn descriptor() -> KitDescriptor {
        KitDescriptor::from_json(json!({
            "schemaVersion": "3", "kind": "mixin", "displayName": "Test", "description": "A test",
            "capabilities": [
                { "type": NETWORK, "config": { "runtime": { "allow": ["a.example.com:443"], "deny": ["b.example.com"] } } },
                { "type": LIFECYCLE, "name": "Setup", "config": {
                    "install": [{ "command": "echo 'hi' > /tmp/x", "user": "agent", "env": ["MCP_GATEWAY_URL"] },
                                { "command": ["/bin/setup", "--quiet"] }],
                    "startup": [{ "command": "date", "background": true }, { "command": ["sh", "-c", "uptime"], "user": "0" }]
                } }
            ]
        }))
        .unwrap()
    }

    /// The artifact of [`descriptor`], as JSON.
    fn artifact() -> Value {
        mixin(&MixinParts {
            name: "test-mixin",
            reference: "/kits/test-mixin",
            descriptor: &descriptor(),
            image: "sbx-kit-src:spec-0123",
            manifest_digest: "sha256:00",
        })
        .into_json()
    }

    #[test]
    fn the_artifact_holds_the_fields_of_the_cli() {
        let artifact = artifact();
        assert_eq!(
            artifact["manifest"],
            json!({ "description": "A test", "displayName": "Test", "kind": "mixin", "name": "test-mixin",
                    "schemaVersion": "3", "template": "sbx-kit-src:spec-0123" })
        );
        assert_eq!(artifact["v3"], descriptor().to_json());
        assert_eq!(artifact["v3Digest"], "sha256:00");
        assert_eq!(artifact["v3Reference"], "/kits/test-mixin");
        assert_eq!(
            artifact["kitUsage"],
            json!({ "kit_identifier": "unknown", "source_type": "local-dir" })
        );
        assert_eq!(
            artifact["caps"]["network"],
            json!({ "allow": ["a.example.com:443"], "deny": ["b.example.com"] })
        );
        let fields: Vec<&String> = artifact.as_object().unwrap().keys().collect();
        assert_eq!(
            fields,
            [
                "caps",
                "commands",
                "kitUsage",
                "manifest",
                "v3",
                "v3Digest",
                "v3Effective",
                "v3Reference",
                "v3Selection"
            ]
        );
    }

    #[test]
    fn install_commands_are_wrapped_and_startup_commands_listed() {
        let commands = &artifact()["commands"];
        let install = commands["install"][0]["command"].as_str().unwrap();
        assert!(
            install.starts_with("env -i PATH=\"$PATH\" HOME=\"$HOME\""),
            "{install}"
        );
        assert!(
            install.ends_with(
                r#"MCP_GATEWAY_URL="$MCP_GATEWAY_URL" sh -c 'echo '\''hi'\'' > /tmp/x'"#
            ),
            "{install}"
        );
        assert_eq!(commands["install"][0]["user"], "agent");
        assert_eq!(
            commands["install"][1],
            json!({ "command": ["/bin/setup", "--quiet"] })
        );
        assert_eq!(
            commands["startup"],
            json!([{ "command": ["sh", "-c", "date"], "background": true },
                   { "command": ["sh", "-c", "uptime"], "user": "0" }])
        );
    }

    #[test]
    fn the_effective_descriptor_and_the_selection_follow_the_cli() {
        let artifact = artifact();
        assert_eq!(
            artifact["v3Effective"]["capabilities"][1]["config"]["install"][0]["command"],
            json!(["sh", "-c", "echo 'hi' > /tmp/x"])
        );
        let selection = &artifact["v3Selection"];
        assert_eq!(
            selection["selected"][1],
            json!({ "decisions": [{ "accepted": true }],
                    "memberSources": [{ "kit": "/kits/test-mixin", "path": "capabilities[1]" }],
                    "members": ["capabilities[1]"], "name": "Setup", "path": "capabilities[1]",
                    "source": { "kit": "/kits/test-mixin", "path": "capabilities[1]" } })
        );
        assert_eq!(
            selection["capabilities"][0]["source"]["kit"],
            "/kits/test-mixin"
        );
        assert_eq!(selection["capabilities"][0]["type"], NETWORK);
        assert!(selection["selected"][0].get("name").is_none());
        assert!(selection.get("skipped").is_none());
    }

    /// The types of the effective capabilities of `descriptor`, given as the
    /// short types of its capabilities in order.
    fn effective_order(descriptor: &[&str]) -> Vec<String> {
        let listed: Vec<Value> = descriptor
            .iter()
            .enumerate()
            .map(|(index, kind)| json!({ "type": format!("com.docker.sandbox/{kind}"), "name": index }))
            .collect();
        let effective = effective(&json!({ "capabilities": listed }));
        capabilities(&effective)
            .map(|c| {
                c["type"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("com.docker.sandbox/")
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn effective_capabilities_take_the_order_of_the_cli() {
        // The selected capabilities of claude, copilot and opencode, in the
        // order of their descriptors, then in the order the CLI wrote them.
        let cases: [(&[&str], &[&str]); 3] = [
            (
                &[
                    "sbx@1",
                    "network-policy@1",
                    "credential@1",
                    "agent-sessions@1",
                    "agent-interactive-sessions@1",
                    "lifecycle@1",
                    "agent-skills@1",
                    "agent-context@2",
                ],
                &[
                    "network-policy@1",
                    "sbx@1",
                    "credential@1",
                    "agent-skills@1",
                    "agent-sessions@1",
                    "agent-interactive-sessions@1",
                    "lifecycle@1",
                    "agent-context@2",
                ],
            ),
            (
                &[
                    "sbx@1",
                    "network-policy@1",
                    "credential@1",
                    "credential@1",
                    "lifecycle@1",
                    "agent-context@2",
                    "agent-sessions@1",
                    "agent-interactive-sessions@1",
                    "acp@1",
                ],
                &[
                    "network-policy@1",
                    "sbx@1",
                    "credential@1",
                    "credential@1",
                    "acp@1",
                    "agent-sessions@1",
                    "agent-interactive-sessions@1",
                    "lifecycle@1",
                    "agent-context@2",
                ],
            ),
            (
                &[
                    "sbx@1",
                    "network-policy@1",
                    "credential@1",
                    "credential@1",
                    "credential@1",
                    "lifecycle@1",
                    "agent-sessions@1",
                    "agent-interactive-sessions@1",
                    "agent-skills@1",
                    "agent-context@2",
                    "acp@1",
                ],
                &[
                    "network-policy@1",
                    "sbx@1",
                    "credential@1",
                    "credential@1",
                    "credential@1",
                    "agent-skills@1",
                    "acp@1",
                    "agent-sessions@1",
                    "agent-interactive-sessions@1",
                    "lifecycle@1",
                    "agent-context@2",
                ],
            ),
        ];
        for (descriptor, cli) in cases {
            assert_eq!(effective_order(descriptor), cli, "{descriptor:?}");
        }

        // Capabilities of one type keep their order; one of a type the CLI
        // does not resolve goes last.
        let listed = json!({ "capabilities": [
            { "type": "com.docker.sandbox/future@1" },
            { "type": "com.docker.sandbox/credential@1", "name": "b" },
            { "type": "com.docker.sandbox/sbx@1" },
            { "type": "com.docker.sandbox/credential@1", "name": "a" }
        ] });
        let names: Vec<Value> = capabilities(&effective(&listed))
            .map(|c| c["name"].clone())
            .collect();
        assert_eq!(names, [Value::Null, json!("b"), json!("a"), Value::Null]);
        assert_eq!(
            effective(&listed)["capabilities"][3]["type"],
            "com.docker.sandbox/future@1"
        );
        assert_eq!(
            effective(&json!({ "kind": "mixin" })),
            json!({ "kind": "mixin" })
        );
    }

    #[test]
    fn rejected_capabilities_are_skipped_with_their_reason() {
        let listed = [json!({ "type": "a" }), json!({ "type": "b", "name": "B" })];
        let selection = to_json(&selection(&listed, "kit", |c| {
            is(c, "b").then(|| "b is left out".to_owned())
        }));
        assert_eq!(
            selection["capabilities"],
            json!([{ "type": "a", "source": { "kit": "kit", "path": "capabilities[0]" } }])
        );
        assert_eq!(
            selection["skipped"][0]["rejected"],
            json!(["capabilities[1]"])
        );
        assert_eq!(
            selection["skipped"][0]["decisions"],
            json!([{ "accepted": false, "message": "b is left out" }])
        );
    }

    #[test]
    fn mixins_without_lifecycle_or_network_get_neither_field() {
        let empty = KitDescriptor::mixin("Empty");
        let artifact = mixin(&MixinParts {
            name: "empty",
            reference: "/kits/empty",
            descriptor: &empty,
            image: "sbx-kit-src:spec-1",
            manifest_digest: "sha256:01",
        })
        .into_json();
        assert!(artifact.get("commands").is_none() && artifact.get("caps").is_none());
        let lifecycle: LifecycleConfig =
            serde_json::from_value(json!({ "startup": [{ "command": "date" }] })).unwrap();
        let startup_only = to_json(&commands([&lifecycle]).unwrap());
        assert!(startup_only.get("install").is_none());
        assert!(commands([&LifecycleConfig::default()]).is_none());
        let install_rules: NetworkConfig =
            serde_json::from_value(json!({ "install": { "allow": ["a:80"] } })).unwrap();
        assert!(
            caps([&install_rules]).is_none(),
            "install-phase rules are not caps"
        );
        assert_eq!(to_json(&KitUsage::published())["source_type"], "hub");
    }
}
