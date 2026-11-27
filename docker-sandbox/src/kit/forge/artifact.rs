//! The kit artifact of a forged mixin: the entry of `kit_artifacts` the daemon
//! composes the mixin from.
//!
//! The fields are the ones the `sbx` CLI writes for a declarative local mixin,
//! as the audit captured them, and the forged artifact equals the CLI's but for
//! the name and digest of the source image. The daemon needs `manifest` (its
//! `template` names the source image), `v3`, from which it derives the
//! permissions and network rules, and `commands`, the install commands it
//! runs: without them the sandbox is created and nothing is installed, without
//! any error. The other fields are written for fidelity.

use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use super::descriptor::{LIFECYCLE, NETWORK};

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

/// What the artifact of a mixin is built from.
pub(super) struct MixinParts<'a> {
    /// Name of the mixin, the last part of its reference.
    pub(super) name: &'a str,
    /// Reference the mixin is listed under in `kits`.
    pub(super) reference: &'a str,
    /// The validated descriptor.
    pub(super) descriptor: &'a Value,
    /// Name of its source image, e.g. `sbx-kit-src:spec-…`.
    pub(super) image: &'a str,
    /// Digest of the manifest of its source image.
    pub(super) manifest_digest: &'a str,
}

/// The kit artifact of a mixin.
pub(super) fn mixin(parts: &MixinParts<'_>) -> Value {
    let descriptor = parts.descriptor;
    let text = |key: &str| descriptor.get(key).cloned().unwrap_or(Value::Null);
    let mut artifact = Map::new();
    artifact.insert(
        "manifest".into(),
        json!({
            "description": text("description"),
            "displayName": text("displayName"),
            "kind": "mixin",
            "name": parts.name,
            "schemaVersion": "3",
            "template": parts.image,
        }),
    );
    artifact.insert("v3".into(), descriptor.clone());
    artifact.insert("v3Effective".into(), effective(descriptor));
    artifact.insert("v3Selection".into(), selection(descriptor, parts.reference));
    artifact.insert("v3Digest".into(), json!(parts.manifest_digest));
    artifact.insert("v3Reference".into(), json!(parts.reference));
    artifact.insert(
        "kitUsage".into(),
        json!({ "kit_identifier": "unknown", "source_type": "local-dir" }),
    );
    if let Some(commands) = commands(descriptor) {
        artifact.insert("commands".into(), commands);
    }
    if let Some(network) = network(descriptor) {
        artifact.insert("caps".into(), json!({ "network": network }));
    }
    Value::Object(artifact)
}

/// The capabilities of `descriptor`, in order.
fn capabilities(descriptor: &Value) -> impl Iterator<Item = &Value> {
    descriptor
        .get("capabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// Whether `capability` is of the type `kind`.
fn is(capability: &Value, kind: &str) -> bool {
    capability.get("type").and_then(Value::as_str) == Some(kind)
}

/// `descriptor` normalized as the CLI does: lifecycle commands written as
/// `["sh", "-c", command]`.
fn effective(descriptor: &Value) -> Value {
    let mut effective = descriptor.clone();
    if let Some(capabilities) = effective
        .get_mut("capabilities")
        .and_then(Value::as_array_mut)
    {
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
    }
    effective
}

/// The capability selection of a mixin: every capability accepted, as the
/// CLI reports it for a kit set without conflicts.
fn selection(descriptor: &Value, reference: &str) -> Value {
    let mut listed = Vec::new();
    let mut selected = Vec::new();
    for (index, capability) in capabilities(descriptor).enumerate() {
        let path = format!("capabilities[{index}]");
        let source = json!({ "kit": reference, "path": path });
        let mut with_source = capability.clone();
        with_source["source"] = source.clone();
        listed.push(with_source);
        let mut entry = json!({
            "decisions": [{ "accepted": true }],
            "memberSources": [source.clone()],
            "members": [path],
            "path": path,
            "source": source,
        });
        if let Some(name) = capability.get("name") {
            entry["name"] = name.clone();
        }
        selected.push(entry);
    }
    json!({ "capabilities": listed, "selected": selected })
}

/// The commands the daemon runs: install commands wrapped in an emptied
/// environment that keeps [`INSTALL_ENV`] and the variables the step names,
/// startup commands as `["sh", "-c", command]`; a command the kit writes as a
/// list is kept as it is. `None` without lifecycle.
fn commands(descriptor: &Value) -> Option<Value> {
    let mut phases = Map::new();
    for capability in capabilities(descriptor).filter(|c| is(c, LIFECYCLE)) {
        for phase in ["install", "startup"] {
            let Some(steps) = capability
                .pointer(&format!("/config/{phase}"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            let entries = phases
                .entry(phase)
                .or_insert_with(|| Value::Array(Vec::new()));
            for step in steps {
                let command = match step.get("command") {
                    Some(Value::String(command)) if phase == "install" => {
                        json!(wrap_install(command, step))
                    }
                    Some(Value::String(command)) => json!(["sh", "-c", command]),
                    Some(list) => list.clone(),
                    None => continue,
                };
                let mut entry = Map::new();
                entry.insert("command".into(), command);
                for key in ["description", "user", "background"] {
                    if let Some(value) = step.get(key) {
                        entry.insert(key.into(), value.clone());
                    }
                }
                if let Value::Array(entries) = entries {
                    entries.push(Value::Object(entry));
                }
            }
        }
    }
    (!phases.is_empty()).then_some(Value::Object(phases))
}

/// `env -i NAME="$NAME" … sh -c '<command>'`, the form the CLI gives install
/// commands.
fn wrap_install(command: &str, step: &Value) -> String {
    let named = step
        .get("env")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    let mut wrapped = String::from("env -i");
    for name in INSTALL_ENV.iter().copied().chain(named) {
        let _ = write!(wrapped, " {name}=\"${name}\"");
    }
    let _ = write!(wrapped, " sh -c '{}'", command.replace('\'', r"'\''"));
    wrapped
}

/// The run-time network rules of the descriptor, as the `caps.network` of the
/// artifact. `None` without any.
fn network(descriptor: &Value) -> Option<Value> {
    let mut rules = Map::new();
    for capability in capabilities(descriptor).filter(|c| is(c, NETWORK)) {
        for decision in ["allow", "deny"] {
            let hosts = capability
                .pointer(&format!("/config/runtime/{decision}"))
                .and_then(Value::as_array);
            for host in hosts.into_iter().flatten() {
                if let Value::Array(list) = rules
                    .entry(decision)
                    .or_insert_with(|| Value::Array(Vec::new()))
                {
                    list.push(host.clone());
                }
            }
        }
    }
    (!rules.is_empty()).then_some(Value::Object(rules))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mixin with network rules, an install command naming a variable, a
    /// startup command in the background and one written as a list.
    fn descriptor() -> Value {
        json!({
            "schemaVersion": "3", "kind": "mixin", "displayName": "Test", "description": "A test",
            "capabilities": [
                { "type": NETWORK, "config": { "runtime": { "allow": ["a.example.com:443"], "deny": ["b.example.com"] } } },
                { "type": LIFECYCLE, "name": "Setup", "config": {
                    "install": [{ "command": "echo 'hi' > /tmp/x", "user": "agent", "env": ["MCP_GATEWAY_URL"] }],
                    "startup": [{ "command": "date", "background": true }, { "command": ["sh", "-c", "uptime"], "user": "0" }]
                } }
            ]
        })
    }

    /// The artifact of [`descriptor`].
    fn artifact() -> Value {
        mixin(&MixinParts {
            name: "test-mixin",
            reference: "/kits/test-mixin",
            descriptor: &descriptor(),
            image: "sbx-kit-src:spec-0123",
            manifest_digest: "sha256:00",
        })
    }

    #[test]
    fn the_artifact_holds_the_fields_of_the_cli() {
        let artifact = artifact();
        assert_eq!(
            artifact["manifest"],
            json!({ "description": "A test", "displayName": "Test", "kind": "mixin", "name": "test-mixin",
                    "schemaVersion": "3", "template": "sbx-kit-src:spec-0123" })
        );
        assert_eq!(artifact["v3"], descriptor());
        assert_eq!(artifact["v3Digest"], "sha256:00");
        assert_eq!(artifact["v3Reference"], "/kits/test-mixin");
        assert_eq!(artifact["kitUsage"]["source_type"], "local-dir");
        assert_eq!(
            artifact["caps"]["network"],
            json!({ "allow": ["a.example.com:443"], "deny": ["b.example.com"] })
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
        assert_eq!(selection["selected"][1]["path"], "capabilities[1]");
        assert_eq!(selection["selected"][1]["name"], "Setup");
        assert_eq!(
            selection["capabilities"][0]["source"]["kit"],
            "/kits/test-mixin"
        );
        assert!(selection["selected"][0].get("name").is_none());
    }

    #[test]
    fn mixins_without_lifecycle_or_network_get_neither_field() {
        let empty = json!({ "schemaVersion": "3", "kind": "mixin" });
        let artifact = mixin(&MixinParts {
            name: "empty",
            reference: "/kits/empty",
            descriptor: &empty,
            image: "sbx-kit-src:spec-1",
            manifest_digest: "sha256:01",
        });
        assert!(artifact.get("commands").is_none() && artifact.get("caps").is_none());
        let lifecycle = json!({ "schemaVersion": "3", "kind": "mixin",
            "capabilities": [{ "type": LIFECYCLE, "config": { "startup": [{ "command": "date" }] } },
                             { "type": NETWORK, "config": { "install": { "allow": ["a:80"] } } }] });
        let startup_only = commands(&lifecycle).unwrap();
        assert!(startup_only.get("install").is_none());
        assert!(
            network(&lifecycle).is_none(),
            "install-phase rules are not caps"
        );
        let no_command = json!({ "capabilities": [{ "type": LIFECYCLE, "config": { "install": [{ "user": "agent" }] } }] });
        assert_eq!(commands(&no_command).unwrap()["install"], json!([]));
    }
}
