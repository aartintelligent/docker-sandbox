//! The artifact of a base kit published as an OCI image, rebuilt from the image
//! itself through the socket, for `kit-forge --base-image`.
//!
//! The CLI resolves a published kit such as `docker.io/sbx/shell:latest` into a
//! kit artifact. Everything it puts there can be read through the socket: the
//! image is pulled (`POST /docker/images/create`) and exported
//! (`GET /docker/images/export`), and the export holds the image index, whose
//! `vnd.docker.sandbox.kit.descriptor` annotation is the descriptor as JSON,
//! and the image configuration, whose entrypoint and command start the agent.
//! The rest is derived: the identity of the kit from its reference and digest,
//! its capability selection from its descriptor, and the values of the
//! arguments it declares (`--base-arg`), which the CLI checks against their
//! pattern or values, substitutes for `${{ kit.args.<name> }}` everywhere but
//! in the raw descriptor, and exports as the variables the kit names.

use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Context, Result, bail};
use docker_sandbox::Client;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::kitforge;

/// Blobs of the export kept in memory: the index, manifests and configuration
/// are a few kilobytes, layers are skipped.
const SMALL_BLOB: u64 = 1 << 20;

/// Agent context capability that cannot compose with [`CONTEXT_V2`].
const CONTEXT_V1: &str = "com.docker.sandbox/agent-context@1";

/// Agent context capability the CLI selects over [`CONTEXT_V1`].
const CONTEXT_V2: &str = "com.docker.sandbox/agent-context@2";

/// Capability declaring a credential the proxy injects.
const CREDENTIAL: &str = "com.docker.sandbox/credential@1";

/// Capability declaring the skills directory of the agent.
const SKILLS: &str = "com.docker.sandbox/agent-skills@1";

/// Mode the CLI gives the shared skills store by default (`skills.defaultMode`).
const SKILLS_MODE: &str = "readonly";

/// A placeholder of a kit argument, `${{ kit.args.<name> }}`.
const ARG_PLACEHOLDER: &str = r"\$\{\{\s*kit\.args\.([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}";

/// A base kit rebuilt from its image.
pub(crate) struct BaseKit {
    /// Reference pinned by digest, the first entry of `kits`, e.g.
    /// `docker.io/sbx/shell@sha256:…`.
    pub reference: String,
    /// Name of the kit, the `agent` of the creation request, e.g. `shell`.
    pub name: String,
    /// The kit artifact.
    pub artifact: Value,
    /// The arguments given, the `kit_args` of the creation request.
    pub kit_args: BTreeMap<String, String>,
}

/// A reference such as `docker.io/sbx/claude:latest`, optionally pinned as
/// `docker.io/sbx/claude:latest@sha256:…`.
struct Reference<'a> {
    /// Repository, e.g. `docker.io/sbx/claude`.
    repository: &'a str,
    /// Tag, `latest` when the reference names none.
    tag: &'a str,
    /// Digest the reference is pinned to, if any.
    digest: Option<&'a str>,
}

/// The arguments of a kit resolved with the values given.
struct ResolvedArgs {
    /// Value of every argument the kit declares, but build arguments.
    values: BTreeMap<String, String>,
    /// Declarations of those arguments, without the variable they set.
    declared: Map<String, Value>,
    /// Variables the arguments set, by name.
    environment: Map<String, Value>,
}

/// What the export of a kit image tells about it.
struct ImageFacts {
    /// Digest of the image index (or manifest) the reference resolves to.
    digest: String,
    /// The descriptor, from the `vnd.docker.sandbox.kit.descriptor` annotation.
    descriptor: Value,
    /// Entrypoint of the image configuration.
    entrypoint: Vec<String>,
    /// Command of the image configuration.
    cmd: Vec<String>,
}

/// Pulls `requested` (e.g. `docker.io/sbx/shell:latest`, or pinned as
/// `docker.io/sbx/shell:latest@sha256:…`), exports it and rebuilds its kit
/// artifact with the argument values `args`.
pub(crate) async fn forge(
    client: &Client,
    requested: &str,
    args: &BTreeMap<String, String>,
) -> Result<BaseKit> {
    let reference = parse_reference(requested);
    let fetched = match reference.digest {
        Some(digest) => format!("{}@{digest}", reference.repository),
        None => format!("{}:{}", reference.repository, reference.tag),
    };
    client
        .images()
        .pull(&fetched)
        .await
        .with_context(|| format!("pulling {fetched}"))?;
    let archive = client
        .images()
        .export(&fetched)
        .await
        .with_context(|| format!("exporting {fetched}"))?;
    let facts = read_export(&archive)?;
    if let Some(digest) = reference.digest
        && digest != facts.digest
    {
        bail!("{fetched} exported as {}", facts.digest);
    }
    base_kit(&reference, &facts, args)
}

/// Reads the digest, descriptor, entrypoint and command of an exported image.
fn read_export(archive: &[u8]) -> Result<ImageFacts> {
    let mut files = BTreeMap::new();
    let mut tar = tar::Archive::new(archive);
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.size() > SMALL_BLOB || !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.display().to_string();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        files.insert(path, bytes);
    }
    let json_file = |path: &str| -> Result<Value> {
        let bytes = files
            .get(path)
            .with_context(|| format!("the export has no {path}"))?;
        serde_json::from_slice(bytes).with_context(|| format!("{path}: invalid JSON"))
    };
    let blob = |digest: &str| format!("blobs/sha256/{}", digest.trim_start_matches("sha256:"));

    let index = json_file("index.json")?;
    let digest = index
        .pointer("/manifests/0/digest")
        .and_then(Value::as_str)
        .context("index.json names no image")?
        .to_owned();
    let top = json_file(&blob(&digest))?;
    let encoded = top
        .pointer("/annotations/vnd.docker.sandbox.kit.descriptor")
        .and_then(Value::as_str)
        .context("the image carries no kit descriptor annotation")?;
    let descriptor: Value =
        serde_json::from_str(encoded).context("the kit descriptor annotation is not JSON")?;

    let config_path = json_file("manifest.json")?
        .pointer("/0/Config")
        .and_then(Value::as_str)
        .context("manifest.json names no configuration")?
        .to_owned();
    let config = json_file(&config_path)?;
    let strings = |pointer: &str| -> Vec<String> {
        config
            .pointer(pointer)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    Ok(ImageFacts {
        digest,
        descriptor,
        entrypoint: strings("/config/Entrypoint"),
        cmd: strings("/config/Cmd"),
    })
}

/// The repository, tag and digest of `reference`; the tag defaults to
/// `latest`.
fn parse_reference(reference: &str) -> Reference<'_> {
    let (named, digest) = match reference.split_once('@') {
        Some((named, digest)) => (named, Some(digest)),
        None => (reference, None),
    };
    let last_slash = named.rfind('/').unwrap_or(0);
    let (repository, tag) = match named.rfind(':') {
        Some(colon) if colon > last_slash => (&named[..colon], &named[colon + 1..]),
        _ => (named, "latest"),
    };
    Reference {
        repository,
        tag,
        digest,
    }
}

/// Resolves the arguments `descriptor` declares with the values `provided`:
/// a value given, else the default; required arguments must be given, and
/// values must match the declared values or pattern. Build arguments, which
/// the published image already carries, are left out.
///
/// # Errors
///
/// Fails on an argument the kit does not declare, a required argument without
/// a value, or a value its values or pattern refuse.
fn resolve_args(descriptor: &Value, provided: &BTreeMap<String, String>) -> Result<ResolvedArgs> {
    let empty = Map::new();
    let declarations = descriptor
        .get("args")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let is_build = |declaration: &Value| declaration.get("buildArg").is_some();
    for name in provided.keys() {
        if declarations.get(name).is_none_or(is_build) {
            bail!("the kit declares no argument {name}");
        }
    }
    let mut resolved = ResolvedArgs {
        values: BTreeMap::new(),
        declared: Map::new(),
        environment: Map::new(),
    };
    for (name, declaration) in declarations {
        if is_build(declaration) {
            continue;
        }
        let value = match (provided.get(name), declaration.get("default")) {
            (Some(value), _) => value.clone(),
            (None, Some(default)) => default.as_str().unwrap_or_default().to_owned(),
            (None, None) if declaration.get("required") == Some(&Value::Bool(true)) => {
                bail!("the kit argument {name} is required")
            }
            (None, None) => String::new(),
        };
        if let Some(values) = declaration.get("enum").and_then(Value::as_array)
            && !values.iter().any(|v| v.as_str() == Some(value.as_str()))
        {
            bail!("{value:?} is not a value of the kit argument {name}");
        }
        if let Some(pattern) = declaration.get("pattern").and_then(Value::as_str) {
            let regex = Regex::new(&format!("^(?:{pattern})$"))
                .with_context(|| format!("pattern of the kit argument {name}"))?;
            if !regex.is_match(&value) {
                bail!("{value:?} does not match the pattern of the kit argument {name}");
            }
        }
        let mut declared = declaration.clone();
        if let Some(variable) = declared
            .as_object_mut()
            .and_then(|fields| fields.remove("env"))
            .and_then(|v| v.as_str().map(str::to_owned))
        {
            resolved.environment.insert(variable, json!(value));
        }
        resolved.declared.insert(name.clone(), declared);
        resolved.values.insert(name.clone(), value);
    }
    Ok(resolved)
}

/// `value` with every `${{ kit.args.<name> }}` replaced by the value of the
/// argument.
///
/// # Errors
///
/// Fails on a placeholder of an argument without a value.
fn substitute(value: &Value, values: &BTreeMap<String, String>) -> Result<Value> {
    let placeholder = Regex::new(ARG_PLACEHOLDER)?;
    let mut unknown = None;
    let substituted = map_strings(value, &mut |text: &str| {
        placeholder
            .replace_all(text, |captures: &regex::Captures<'_>| {
                let name = &captures[1];
                values.get(name).cloned().unwrap_or_else(|| {
                    unknown = Some(name.to_owned());
                    captures[0].to_owned()
                })
            })
            .into_owned()
    });
    match unknown {
        Some(name) => bail!("the kit uses the undeclared argument {name}"),
        None => Ok(substituted),
    }
}

/// `value` with `change` applied to every string it holds.
fn map_strings(value: &Value, change: &mut impl FnMut(&str) -> String) -> Value {
    match value {
        Value::String(text) => Value::String(change(text)),
        Value::Array(items) => Value::Array(items.iter().map(|v| map_strings(v, change)).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), map_strings(v, change)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The base kit `reference` names, from the facts its export gave and the
/// argument values `args`.
fn base_kit(
    reference: &Reference<'_>,
    facts: &ImageFacts,
    args: &BTreeMap<String, String>,
) -> Result<BaseKit> {
    let descriptor = &facts.descriptor;
    let repository = reference.repository;
    if descriptor.get("schemaVersion").and_then(Value::as_str) != Some("3") {
        bail!("{repository}: only v3 kits are rebuilt");
    }
    if descriptor.get("kind").and_then(Value::as_str) != Some("workload") {
        bail!("{repository}: not a workload kit");
    }
    let name = repository
        .rsplit('/')
        .next()
        .unwrap_or(repository)
        .to_owned();
    let pinned = format!("{repository}@{}", facts.digest);
    let source = format!("{repository}:{}", reference.tag);
    let resolved = resolve_args(descriptor, args)?;
    let substituted = substitute(descriptor, &resolved.values)?;

    let capabilities = substituted
        .get("capabilities")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let has_v2 = capabilities
        .iter()
        .any(|c| c.get("type").and_then(Value::as_str) == Some(CONTEXT_V2));
    let rejected = |c: &Value| has_v2 && c.get("type").and_then(Value::as_str) == Some(CONTEXT_V1);
    let context = capabilities.iter().filter(|c| !rejected(c)).find(|c| {
        matches!(
            c.get("type").and_then(Value::as_str),
            Some(CONTEXT_V1 | CONTEXT_V2)
        )
    });

    let mut kept = substituted.clone();
    kept["capabilities"] = Value::Array(
        capabilities
            .iter()
            .filter(|c| !rejected(c))
            .cloned()
            .collect(),
    );
    let mut artifact = Map::new();
    artifact.insert(
        "manifest".into(),
        manifest(descriptor, &name, &pinned, facts, context),
    );
    artifact.insert("v3".into(), descriptor.clone());
    artifact.insert("v3Effective".into(), effective(&kept));
    artifact.insert(
        "v3Selection".into(),
        selection(capabilities, &source, rejected),
    );
    artifact.insert("v3Digest".into(), json!(facts.digest));
    artifact.insert("v3Reference".into(), json!(pinned));
    artifact.insert("v3Entrypoint".into(), json!(facts.entrypoint));
    artifact.insert("storageSource".into(), json!(format!("oci://{source}")));
    artifact.insert(
        "kitUsage".into(),
        json!({ "kit_identifier": "unknown", "source_type": "hub" }),
    );
    if let Some(hub) = repository.strip_prefix("docker.io/") {
        artifact.insert(
            "kitHubPublication".into(),
            json!({ "digest": facts.digest, "repository": hub }),
        );
    }
    if let Some(licenses) = descriptor.get("licenses") {
        artifact.insert("licenses".into(), licenses.clone());
    }
    if let Some(content) = context.and_then(|c| c.pointer("/config/content")) {
        artifact.insert("agentContext".into(), content.clone());
    }
    if let Some(commands) = kitforge::commands(&kept) {
        artifact.insert("commands".into(), commands);
    }
    if let Some(network) = kitforge::network(&kept) {
        artifact.insert("caps".into(), json!({ "network": network }));
    }
    insert_declared_access(&mut artifact, &kept);
    if !resolved.declared.is_empty() {
        artifact.insert("args".into(), Value::Object(resolved.declared));
        artifact.insert("v3ArgValues".into(), json!(resolved.values));
    }
    if !resolved.environment.is_empty() {
        artifact.insert(
            "environment".into(),
            json!({ "variables": resolved.environment }),
        );
    }
    Ok(BaseKit {
        reference: pinned,
        name,
        artifact: Value::Object(artifact),
        kit_args: args.clone(),
    })
}

/// The descriptor `kept` (its selected capabilities, arguments substituted)
/// normalized as the CLI does: lifecycle commands as `["sh", "-c", command]`,
/// API keys named (`""` when the kit names none).
fn effective(kept: &Value) -> Value {
    let mut effective = kitforge::effective(kept);
    if let Some(capabilities) = effective
        .get_mut("capabilities")
        .and_then(Value::as_array_mut)
    {
        for capability in capabilities {
            if let Some(api_key) = capability
                .pointer_mut("/config/apiKey")
                .and_then(Value::as_object_mut)
            {
                api_key.entry("name").or_insert_with(|| json!(""));
            }
        }
    }
    effective
}

/// The `manifest` of a base kit artifact: the identity of the kit, the program
/// of its agent and the options it starts with (the entrypoint and command of
/// its image), and the file its selected agent `context` goes to.
fn manifest(
    descriptor: &Value,
    name: &str,
    pinned: &str,
    facts: &ImageFacts,
    context: Option<&Value>,
) -> Value {
    let text = |key: &str| descriptor.get(key).cloned().unwrap_or(Value::Null);
    let mut manifest = json!({
        "description": text("description"),
        "displayName": text("displayName"),
        "kind": "sandbox",
        "name": name,
        "schemaVersion": "3",
        "sourceURL": text("sourceUrl"),
        "template": pinned,
    });
    if let Some(binary) = facts.entrypoint.first() {
        manifest["binary"] = json!(binary);
        let run_options: Vec<&String> = facts.entrypoint.iter().skip(1).chain(&facts.cmd).collect();
        manifest["runOptions"] = json!(run_options);
    }
    if let Some(filename) = context.and_then(|c| c.pointer("/config/filename")) {
        manifest["aiFilename"] = filename.clone();
    }
    manifest
}

/// Inserts into `artifact` the credentials and the skills directory the
/// selected capabilities of `kept` declare, as the CLI summarizes them:
/// `credentials` and `optionalCredentials`, `skillsRequests`. A credential is
/// a declaration only: the value comes from a secret bound to its service.
fn insert_declared_access(artifact: &mut Map<String, Value>, kept: &Value) {
    let mut credentials = Vec::new();
    let mut optional_credentials = Vec::new();
    let mut skills = Vec::new();
    for capability in kept
        .get("capabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let optional = capability.get("optional") == Some(&Value::Bool(true));
        match capability.get("type").and_then(Value::as_str) {
            Some(CREDENTIAL) => {
                let config = capability.get("config").cloned().unwrap_or_default();
                let mut credential = json!({ "service": config.get("service") });
                if let Some(description) = capability.get("description") {
                    credential["description"] = description.clone();
                }
                if let Some(api_key) = config.get("apiKey") {
                    let mut api_key = api_key.clone();
                    if let Some(fields) = api_key.as_object_mut() {
                        fields.entry("name").or_insert_with(|| json!(""));
                    }
                    credential["apiKey"] = api_key;
                }
                if let Some(oauth) = config.get("oauth") {
                    credential["oauth"] = oauth.clone();
                }
                if optional && let Some(service) = config.get("service") {
                    optional_credentials.push(service.clone());
                }
                credentials.push(credential);
            }
            Some(SKILLS) => skills.push(json!({
                "mode": SKILLS_MODE,
                "optional": optional,
                "path": capability.pointer("/config/path"),
            })),
            _ => {}
        }
    }
    if !credentials.is_empty() {
        artifact.insert("credentials".into(), Value::Array(credentials));
    }
    if !optional_credentials.is_empty() {
        artifact.insert(
            "optionalCredentials".into(),
            Value::Array(optional_credentials),
        );
    }
    if !skills.is_empty() {
        artifact.insert("skillsRequests".into(), Value::Array(skills));
    }
}

/// The capability selection of a base kit: every capability accepted but the
/// `rejected` ones, which the CLI reports as skipped.
fn selection(capabilities: &[Value], source: &str, rejected: impl Fn(&Value) -> bool) -> Value {
    let mut selected_capabilities = Vec::new();
    let mut selected = Vec::new();
    let mut skipped = Vec::new();
    for (index, capability) in capabilities.iter().enumerate() {
        let path = format!("capabilities[{index}]");
        let origin = json!({ "kit": source, "path": path });
        let mut entry = json!({
            "memberSources": [origin.clone()],
            "members": [path],
            "path": path,
            "source": origin,
        });
        if let Some(name) = capability.get("name") {
            entry["name"] = name.clone();
        }
        if rejected(capability) {
            entry["decisions"] = json!([{
                "accepted": false,
                "message": format!(
                    "{CONTEXT_V1} is not selected: this kit set uses {CONTEXT_V2}, and the two cannot compose"
                ),
            }]);
            entry["rejected"] = json!([path]);
            skipped.push(entry);
        } else {
            entry["decisions"] = json!([{ "accepted": true }]);
            let mut with_source = capability.clone();
            with_source["source"] = entry["source"].clone();
            selected_capabilities.push(with_source);
            selected.push(entry);
        }
    }
    let mut selection = json!({ "capabilities": selected_capabilities, "selected": selected });
    if !skipped.is_empty() {
        selection["skipped"] = Value::Array(skipped);
    }
    selection
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The facts of a shell-like kit image.
    fn shell() -> ImageFacts {
        ImageFacts {
            digest: "sha256:6bde".into(),
            descriptor: json!({
                "schemaVersion": "3",
                "kind": "workload",
                "displayName": "Shell",
                "description": "A plain shell",
                "sourceUrl": "https://example.com/definitions",
                "licenses": ["Apache-2.0"],
                "capabilities": [
                    { "type": "com.docker.sandbox/sbx@1" },
                    { "type": CONTEXT_V1, "optional": true, "config": { "content": "one", "filename": "AGENTS.md" } },
                    { "type": CONTEXT_V2, "optional": true, "config": { "content": "two", "filename": "AGENTS.md" } }
                ]
            }),
            entrypoint: vec![
                "bash".into(),
                "-c".into(),
                "exec \"$@\"".into(),
                "bash".into(),
            ],
            cmd: vec!["bash".into(), "-l".into()],
        }
    }

    /// The facts of a claude-like kit image: arguments, a credential, skills,
    /// network rules and lifecycle commands.
    fn agent() -> ImageFacts {
        let mut facts = shell();
        facts.descriptor = json!({
            "schemaVersion": "3",
            "kind": "workload",
            "args": {
                "model": { "default": "", "env": "AGENT_MODEL", "pattern": "^[A-Za-z0-9._-]*$" },
                "effort": { "default": "", "env": "AGENT_EFFORT", "enum": ["", "low", "high"] },
                "version": { "buildArg": "VERSION", "required": true }
            },
            "capabilities": [
                { "type": "com.docker.sandbox/network-policy@1",
                  "config": { "runtime": { "allow": ["api.example.com:443"] } } },
                { "type": CREDENTIAL, "optional": true, "description": "API access",
                  "config": { "service": "example", "phase": "runtime",
                              "apiKey": { "inject": [{ "domain": "api.example.com", "header": "x-api-key", "format": "%s" }] } } },
                { "type": "com.docker.sandbox/lifecycle@1",
                  "config": { "install": [{ "command": "echo '${{ kit.args.model }}' > /tmp/model", "user": "agent" }] } },
                { "type": SKILLS, "optional": true, "config": { "path": "/home/agent/.agent/skills" } }
            ]
        });
        facts
    }

    /// The arguments `pairs` as a map.
    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn references_are_split_into_repository_tag_and_digest() {
        let reference = parse_reference("docker.io/sbx/shell:latest");
        assert_eq!(
            (reference.repository, reference.tag, reference.digest),
            ("docker.io/sbx/shell", "latest", None)
        );
        let reference = parse_reference("docker.io/sbx/claude:latest@sha256:8e4b");
        assert_eq!(
            (reference.repository, reference.tag, reference.digest),
            ("docker.io/sbx/claude", "latest", Some("sha256:8e4b"))
        );
        let reference = parse_reference("localhost:5000/kits/tools");
        assert_eq!(
            (reference.repository, reference.tag),
            ("localhost:5000/kits/tools", "latest")
        );
    }

    #[test]
    fn the_base_artifact_holds_the_fields_of_the_cli() {
        let reference = parse_reference("docker.io/sbx/shell:latest");
        let kit = base_kit(&reference, &shell(), &BTreeMap::new()).unwrap();
        assert_eq!(kit.reference, "docker.io/sbx/shell@sha256:6bde");
        assert_eq!(kit.name, "shell");
        let artifact = &kit.artifact;
        assert_eq!(artifact["manifest"]["binary"], "bash");
        assert_eq!(
            artifact["manifest"]["runOptions"],
            json!(["-c", "exec \"$@\"", "bash", "bash", "-l"])
        );
        assert_eq!(artifact["manifest"]["aiFilename"], "AGENTS.md");
        assert_eq!(artifact["agentContext"], "two");
        assert_eq!(
            artifact["storageSource"],
            "oci://docker.io/sbx/shell:latest"
        );
        assert_eq!(artifact["kitHubPublication"]["repository"], "sbx/shell");
        assert_eq!(
            artifact["v3Effective"]["capabilities"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "agent-context@1 is left out"
        );
        assert_eq!(
            artifact["v3Selection"]["skipped"][0]["path"],
            "capabilities[1]"
        );
        assert!(artifact.get("args").is_none() && artifact.get("commands").is_none());
    }

    #[test]
    fn arguments_are_resolved_checked_and_substituted() {
        let reference = parse_reference("docker.io/sbx/agent");
        let kit = base_kit(
            &reference,
            &agent(),
            &args(&[("model", "small-1"), ("effort", "low")]),
        )
        .unwrap();
        let artifact = &kit.artifact;
        assert_eq!(
            artifact["v3ArgValues"],
            json!({ "effort": "low", "model": "small-1" })
        );
        assert_eq!(
            artifact["environment"]["variables"],
            json!({ "AGENT_EFFORT": "low", "AGENT_MODEL": "small-1" })
        );
        assert!(
            artifact["args"].get("version").is_none(),
            "build arguments are left out"
        );
        assert!(artifact["args"]["model"].get("env").is_none());
        let install = artifact["commands"]["install"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            install.ends_with(r"sh -c 'echo '\''small-1'\'' > /tmp/model'"),
            "{install}"
        );
        assert!(
            artifact["v3"].to_string().contains("${{ kit.args.model }}"),
            "the raw descriptor keeps its placeholders"
        );
        assert_eq!(kit.kit_args["model"], "small-1");

        let defaults = base_kit(&reference, &agent(), &BTreeMap::new()).unwrap();
        assert_eq!(
            defaults.artifact["environment"]["variables"]["AGENT_MODEL"],
            ""
        );

        for bad in [
            args(&[("effort", "max")]),
            args(&[("model", "it's")]),
            args(&[("version", "1.0.0")]),
            args(&[("unknown", "x")]),
        ] {
            assert!(base_kit(&reference, &agent(), &bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn credentials_skills_and_network_are_summarized() {
        let reference = parse_reference("docker.io/sbx/agent");
        let kit = base_kit(&reference, &agent(), &BTreeMap::new()).unwrap();
        let artifact = &kit.artifact;
        assert_eq!(artifact["credentials"][0]["service"], "example");
        assert_eq!(artifact["credentials"][0]["apiKey"]["name"], "");
        assert!(artifact["credentials"][0].get("phase").is_none());
        assert_eq!(artifact["optionalCredentials"], json!(["example"]));
        assert_eq!(
            artifact["skillsRequests"],
            json!([{ "mode": "readonly", "optional": true, "path": "/home/agent/.agent/skills" }])
        );
        assert_eq!(
            artifact["caps"]["network"]["allow"],
            json!(["api.example.com:443"])
        );
        assert_eq!(
            artifact["v3Effective"]["capabilities"][1]["config"]["apiKey"]["name"],
            ""
        );
    }

    #[test]
    fn only_v3_workloads_are_rebuilt() {
        let reference = parse_reference("docker.io/sbx/shell");
        let mut facts = shell();
        facts.descriptor["kind"] = json!("mixin");
        assert!(base_kit(&reference, &facts, &BTreeMap::new()).is_err());
        facts.descriptor["schemaVersion"] = json!("2");
        assert!(base_kit(&reference, &facts, &BTreeMap::new()).is_err());
    }
}
