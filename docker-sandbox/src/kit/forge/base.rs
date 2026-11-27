//! Base kits rebuilt from their published image, through the socket alone.
//!
//! The `sbx` CLI resolves a published kit such as `docker.io/sbx/shell:latest`
//! into a kit artifact. Everything it puts there can be read through the
//! socket: the image is pulled (`POST /docker/images/create`) and exported
//! (`GET /docker/images/export`); the export holds the image index, whose
//! `vnd.docker.sandbox.kit.descriptor` annotation is the descriptor as JSON,
//! and the image configuration, whose entrypoint and command start the agent.
//! The rest is derived as the CLI does, as the audit of this repository
//! measured it: the identity of the kit from its reference and digest, its
//! capability selection from its descriptor, and its arguments
//! ([`super::args`]).
//!
//! The selection knows one rule: `agent-context@1` is left out when the kit
//! also declares `agent-context@2`, the only conflict published kits hold.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::artifact::{capabilities, commands, effective, is, network};
use super::{args, export};
use crate::client::Client;
use crate::error::{Error, Result};

/// Annotation of the image index that holds the descriptor as JSON.
const DESCRIPTOR_ANNOTATION: &str = "vnd.docker.sandbox.kit.descriptor";

/// Registry of references that name none, as Docker normalizes them.
const DEFAULT_REGISTRY: &str = "docker.io";

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

/// A kit reference, normalized as Docker does: `sbx/shell` is
/// `docker.io/sbx/shell:latest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Reference {
    /// Repository, registry included, e.g. `docker.io/sbx/claude`.
    pub(super) repository: String,
    /// Tag, `latest` when the reference names none.
    tag: String,
    /// Digest the reference is pinned to, if any.
    digest: Option<String>,
}

impl Reference {
    /// Parses `reference`, e.g. `docker.io/sbx/claude:latest`, optionally
    /// pinned as `docker.io/sbx/claude:latest@sha256:…`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidKit`] when the reference is empty, holds a
    /// space, or is pinned to something other than a SHA-256 digest.
    pub(super) fn parse(reference: &str) -> Result<Self> {
        let invalid = |message: &str| Error::InvalidKit {
            kit: reference.to_owned(),
            message: message.to_owned(),
        };
        if reference.is_empty() || reference.contains(char::is_whitespace) {
            return Err(invalid("a kit reference is an image name without spaces"));
        }
        let (named, digest) = match reference.split_once('@') {
            Some((named, digest)) if digest.starts_with("sha256:") => (named, Some(digest)),
            Some(_) => return Err(invalid("a kit is pinned by a sha256 digest")),
            None => (reference, None),
        };
        let last_slash = named.rfind('/');
        let (path, tag) = match named.rfind(':') {
            Some(colon) if last_slash.is_none_or(|slash| colon > slash) => {
                (&named[..colon], &named[colon + 1..])
            }
            _ => (named, "latest"),
        };
        let first = path.split('/').next().unwrap_or_default();
        let has_registry = path.contains('/')
            && (first.contains('.') || first.contains(':') || first == "localhost");
        let repository = match (has_registry, path.contains('/')) {
            (true, _) => path.to_owned(),
            (false, true) => format!("{DEFAULT_REGISTRY}/{path}"),
            (false, false) => format!("{DEFAULT_REGISTRY}/library/{path}"),
        };
        Ok(Self {
            repository,
            tag: tag.to_owned(),
            digest: digest.map(str::to_owned),
        })
    }

    /// The reference pulled and exported: by digest when pinned, else by tag.
    fn fetched(&self) -> String {
        match &self.digest {
            Some(digest) => format!("{}@{digest}", self.repository),
            None => format!("{}:{}", self.repository, self.tag),
        }
    }

    /// Name of the kit, the last part of its repository, e.g. `claude`.
    fn name(&self) -> &str {
        self.repository
            .rsplit('/')
            .next()
            .unwrap_or(&self.repository)
    }
}

/// A base kit rebuilt from its image.
#[derive(Debug)]
pub(super) struct BaseKit {
    /// Reference pinned by digest, the first entry of `kits`, e.g.
    /// `docker.io/sbx/shell@sha256:…`.
    pub(super) reference: String,
    /// Name of the kit, the `agent` of the creation request, e.g. `shell`.
    pub(super) name: String,
    /// The kit artifact.
    pub(super) artifact: Value,
}

/// What the export of a kit image tells about it.
#[derive(Debug, Clone)]
struct ImageFacts {
    /// Digest of the image index (or manifest) the reference resolves to.
    digest: String,
    /// The descriptor, from the [`DESCRIPTOR_ANNOTATION`] annotation.
    descriptor: Value,
    /// Entrypoint of the image configuration.
    entrypoint: Vec<String>,
    /// Command of the image configuration.
    cmd: Vec<String>,
}

/// Pulls the image of `reference`, exports it and rebuilds its kit artifact
/// with the argument values `args`.
///
/// # Errors
///
/// Returns the errors of the pull and export requests; [`Error::Archive`]
/// when the export is not a tar archive; [`Error::InvalidKit`] when the image
/// is not a v3 workload kit or refuses the arguments; [`Error::Forge`] when a
/// pinned reference exports another digest.
pub(super) async fn rebuild(
    client: &Client,
    reference: &Reference,
    args: &BTreeMap<String, String>,
) -> Result<BaseKit> {
    let fetched = reference.fetched();
    client.images().pull(&fetched).await?;
    let files = export::small_files(client, &fetched).await?;
    let facts = facts(&fetched, &files)?;
    if let Some(digest) = &reference.digest
        && *digest != facts.digest
    {
        return Err(Error::Forge(format!(
            "{fetched} was exported as {}",
            facts.digest
        )));
    }
    base_kit(reference, &facts, args)
}

/// The digest, descriptor, entrypoint and command of the image of `kit`,
/// from the small files of its export.
fn facts(kit: &str, files: &BTreeMap<String, Vec<u8>>) -> Result<ImageFacts> {
    let invalid = |message: String| Error::InvalidKit {
        kit: kit.to_owned(),
        message,
    };
    let json_file = |path: &str| -> Result<Value> {
        let bytes = files
            .get(path)
            .ok_or_else(|| invalid(format!("the export of the image holds no {path}")))?;
        serde_json::from_slice(bytes)
            .map_err(|error| invalid(format!("{path} of the image is not JSON: {error}")))
    };
    let blob = |digest: &str| format!("blobs/sha256/{}", digest.trim_start_matches("sha256:"));

    let digest = json_file("index.json")?
        .pointer("/manifests/0/digest")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("the index of the image names no image".to_owned()))?
        .to_owned();
    let top = json_file(&blob(&digest))?;
    let encoded = top
        .get("annotations")
        .and_then(|annotations| annotations.get(DESCRIPTOR_ANNOTATION))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            invalid("the image is not a kit: it carries no kit descriptor".to_owned())
        })?;
    let descriptor: Value = serde_json::from_str(encoded).map_err(|error| {
        invalid(format!(
            "the kit descriptor of the image is not JSON: {error}"
        ))
    })?;

    let config_path = json_file("manifest.json")?
        .pointer("/0/Config")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("the export of the image names no configuration".to_owned()))?
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

/// The base kit `reference` names, from the facts its export gave and the
/// argument values `args`.
fn base_kit(
    reference: &Reference,
    facts: &ImageFacts,
    args: &BTreeMap<String, String>,
) -> Result<BaseKit> {
    let descriptor = &facts.descriptor;
    let repository = reference.repository.as_str();
    let invalid = |message: &str| Error::InvalidKit {
        kit: repository.to_owned(),
        message: message.to_owned(),
    };
    if descriptor.get("schemaVersion").and_then(Value::as_str) != Some("3") {
        return Err(invalid("only v3 kits are rebuilt"));
    }
    if descriptor.get("kind").and_then(Value::as_str) != Some("workload") {
        return Err(invalid("a base kit is a workload kit"));
    }
    let name = reference.name().to_owned();
    let pinned = format!("{repository}@{}", facts.digest);
    let source = format!("{repository}:{}", reference.tag);
    let resolved = args::resolve(repository, descriptor, args)?;
    let substituted = args::substitute(repository, descriptor, &resolved.values)?;

    let listed: Vec<&Value> = capabilities(&substituted).collect();
    let has_v2 = listed.iter().any(|c| is(c, CONTEXT_V2));
    let rejected = |c: &Value| has_v2 && is(c, CONTEXT_V1);
    let context = listed
        .iter()
        .copied()
        .find(|c| !rejected(c) && (is(c, CONTEXT_V1) || is(c, CONTEXT_V2)));
    let mut kept = substituted.clone();
    kept["capabilities"] = listed
        .iter()
        .copied()
        .filter(|c| !rejected(c))
        .cloned()
        .collect();

    let mut artifact = Map::new();
    artifact.insert(
        "manifest".into(),
        manifest(descriptor, &name, &pinned, facts, context),
    );
    artifact.insert("v3".into(), descriptor.clone());
    artifact.insert("v3Effective".into(), with_key_names(effective(&kept)));
    artifact.insert("v3Selection".into(), selection(&listed, &source, rejected));
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
    if let Some(commands) = commands(&kept) {
        artifact.insert("commands".into(), commands);
    }
    if let Some(network) = network(&kept) {
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
    })
}

/// `effective` with the API keys of its capabilities named, `""` when the kit
/// names none, as the CLI writes them.
fn with_key_names(mut effective: Value) -> Value {
    let capabilities = effective
        .get_mut("capabilities")
        .and_then(Value::as_array_mut);
    for capability in capabilities.into_iter().flatten() {
        if let Some(api_key) = capability
            .pointer_mut("/config/apiKey")
            .and_then(Value::as_object_mut)
        {
            api_key.entry("name").or_insert_with(|| json!(""));
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
/// a declaration only: no value goes into the package.
fn insert_declared_access(artifact: &mut Map<String, Value>, kept: &Value) {
    let mut credentials = Vec::new();
    let mut optional_credentials = Vec::new();
    let mut skills = Vec::new();
    for capability in capabilities(kept) {
        let optional = capability.get("optional") == Some(&Value::Bool(true));
        if is(capability, CREDENTIAL) {
            let config = capability.get("config").cloned().unwrap_or_default();
            let mut credential = json!({ "service": config.get("service") });
            if let Some(description) = capability.get("description") {
                credential["description"] = description.clone();
            }
            if let Some(api_key) = config.get("apiKey") {
                credential["apiKey"] = with_name(api_key.clone());
            }
            if let Some(oauth) = config.get("oauth") {
                credential["oauth"] = oauth.clone();
            }
            if optional && let Some(service) = config.get("service") {
                optional_credentials.push(service.clone());
            }
            credentials.push(credential);
        } else if is(capability, SKILLS) {
            skills.push(json!({
                "mode": SKILLS_MODE,
                "optional": optional,
                "path": capability.pointer("/config/path"),
            }));
        }
    }
    for (key, list) in [
        ("credentials", credentials),
        ("optionalCredentials", optional_credentials),
        ("skillsRequests", skills),
    ] {
        if !list.is_empty() {
            artifact.insert(key.into(), Value::Array(list));
        }
    }
}

/// `api_key` named `""` when it names nothing.
fn with_name(mut api_key: Value) -> Value {
    if let Some(fields) = api_key.as_object_mut() {
        fields.entry("name").or_insert_with(|| json!(""));
    }
    api_key
}

/// The capability selection of a base kit: every capability accepted but the
/// `rejected` ones, which the CLI reports as skipped.
fn selection(listed: &[&Value], source: &str, rejected: impl Fn(&Value) -> bool) -> Value {
    let mut selected_capabilities = Vec::new();
    let mut selected = Vec::new();
    let mut skipped = Vec::new();
    for (index, capability) in listed.iter().enumerate() {
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
            let mut with_source = (*capability).clone();
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
                    { "type": "com.docker.sandbox/sbx@1", "name": "sbx" },
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

    /// The facts of an agent-like kit image: arguments, a credential, skills,
    /// network rules and lifecycle commands.
    fn agent() -> ImageFacts {
        let mut facts = shell();
        facts.descriptor = json!({
            "schemaVersion": "3",
            "kind": "workload",
            "args": {
                "model": { "default": "", "env": "AGENT_MODEL", "pattern": "^[A-Za-z0-9._-]*$" },
                "version": { "buildArg": "VERSION", "required": true }
            },
            "capabilities": [
                { "type": "com.docker.sandbox/network-policy@1",
                  "config": { "runtime": { "allow": ["api.example.com:443"] } } },
                { "type": CREDENTIAL, "optional": true, "description": "API access",
                  "config": { "service": "example", "phase": "runtime",
                              "apiKey": { "inject": [{ "domain": "api.example.com", "header": "x-api-key", "format": "%s" }] },
                              "oauth": { "provider": "example" } } },
                { "type": CREDENTIAL, "config": { "service": "other", "apiKey": "opaque" } },
                { "type": "com.docker.sandbox/lifecycle@1",
                  "config": { "install": [{ "command": "echo '${{ kit.args.model }}' > /tmp/model", "user": "agent" }] } },
                { "type": SKILLS, "optional": true, "config": { "path": "/home/agent/.agent/skills" } }
            ]
        });
        facts
    }

    /// The arguments `pairs` as a map.
    fn given(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// The reference `text`, parsed.
    fn reference(text: &str) -> Reference {
        Reference::parse(text).unwrap()
    }

    #[test]
    fn references_are_normalized_as_docker_does() {
        let parsed = reference("docker.io/sbx/claude:latest@sha256:8e4b");
        assert_eq!(parsed.repository, "docker.io/sbx/claude");
        assert_eq!(parsed.fetched(), "docker.io/sbx/claude@sha256:8e4b");
        assert_eq!(parsed.name(), "claude");
        let cases = [
            ("sbx/shell", "docker.io/sbx/shell:latest"),
            ("ubuntu:24.04", "docker.io/library/ubuntu:24.04"),
            (
                "localhost:5000/kits/tools",
                "localhost:5000/kits/tools:latest",
            ),
            ("localhost/tools:1", "localhost/tools:1"),
            ("ghcr.io/acme/tools:2", "ghcr.io/acme/tools:2"),
        ];
        for (text, fetched) in cases {
            assert_eq!(reference(text).fetched(), fetched, "{text}");
        }
        for bad in [
            "",
            "docker.io/sbx/shell latest",
            "docker.io/sbx/shell@md5:00",
        ] {
            assert!(
                matches!(Reference::parse(bad), Err(Error::InvalidKit { .. })),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_base_artifact_holds_the_fields_of_the_cli() {
        let kit = base_kit(
            &reference("docker.io/sbx/shell:latest"),
            &shell(),
            &BTreeMap::new(),
        )
        .unwrap();
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
        assert_eq!(artifact["licenses"], json!(["Apache-2.0"]));
        assert_eq!(
            artifact["v3Effective"]["capabilities"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            artifact["v3Selection"]["skipped"][0]["path"],
            "capabilities[1]"
        );
        assert_eq!(artifact["v3Selection"]["selected"][0]["name"], "sbx");
        for absent in ["args", "commands", "caps", "credentials", "environment"] {
            assert!(artifact.get(absent).is_none(), "{absent}");
        }

        // Without an entrypoint, an agent context or a Docker Hub repository.
        let mut bare = shell();
        bare.entrypoint.clear();
        bare.descriptor["capabilities"] = json!([]);
        let kit = base_kit(&reference("ghcr.io/acme/tools"), &bare, &BTreeMap::new()).unwrap();
        let artifact = &kit.artifact;
        assert!(artifact["manifest"].get("binary").is_none());
        assert!(artifact["manifest"].get("aiFilename").is_none());
        assert!(
            artifact.get("kitHubPublication").is_none() && artifact.get("agentContext").is_none()
        );
        assert!(artifact["v3Selection"].get("skipped").is_none());
    }

    #[test]
    fn arguments_are_resolved_and_substituted() {
        let kit = base_kit(
            &reference("docker.io/sbx/agent"),
            &agent(),
            &given(&[("model", "small-1")]),
        )
        .unwrap();
        let artifact = &kit.artifact;
        assert_eq!(artifact["v3ArgValues"], json!({ "model": "small-1" }));
        assert_eq!(
            artifact["environment"]["variables"],
            json!({ "AGENT_MODEL": "small-1" })
        );
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
        let refused = base_kit(
            &reference("docker.io/sbx/agent"),
            &agent(),
            &given(&[("model", "it's")]),
        );
        assert!(matches!(refused, Err(Error::InvalidKit { .. })));
        let mut undeclared = agent();
        undeclared.descriptor["displayName"] = json!("${{ kit.args.version }}");
        assert!(
            base_kit(
                &reference("docker.io/sbx/agent"),
                &undeclared,
                &BTreeMap::new()
            )
            .is_err()
        );
    }

    #[test]
    fn credentials_skills_and_network_are_summarized() {
        let kit = base_kit(
            &reference("docker.io/sbx/agent"),
            &agent(),
            &BTreeMap::new(),
        )
        .unwrap();
        let artifact = &kit.artifact;
        let credential = &artifact["credentials"][0];
        assert_eq!(credential["service"], "example");
        assert_eq!(credential["description"], "API access");
        assert_eq!(credential["apiKey"]["name"], "");
        assert_eq!(credential["oauth"]["provider"], "example");
        assert!(credential.get("phase").is_none());
        assert_eq!(artifact["credentials"][1]["apiKey"], "opaque");
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
        let mut facts = shell();
        facts.descriptor["kind"] = json!("mixin");
        let error = base_kit(&reference("sbx/shell"), &facts, &BTreeMap::new()).unwrap_err();
        assert!(
            error.to_string().contains("a base kit is a workload kit"),
            "{error}"
        );
        facts.descriptor["schemaVersion"] = json!("2");
        let error = base_kit(&reference("sbx/shell"), &facts, &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("only v3 kits"), "{error}");
    }

    /// The small files of the export of an image whose index annotations are
    /// `annotations` and whose configuration is `config`.
    fn export(annotations: &Value, config: &Value) -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            (
                "index.json".to_owned(),
                json!({ "manifests": [{ "digest": "sha256:aa" }] })
                    .to_string()
                    .into_bytes(),
            ),
            (
                "blobs/sha256/aa".to_owned(),
                json!({ "annotations": annotations })
                    .to_string()
                    .into_bytes(),
            ),
            (
                "manifest.json".to_owned(),
                json!([{ "Config": "blobs/sha256/cc" }])
                    .to_string()
                    .into_bytes(),
            ),
            (
                "blobs/sha256/cc".to_owned(),
                config.to_string().into_bytes(),
            ),
        ])
    }

    #[test]
    fn the_facts_of_an_image_come_from_its_export() {
        let descriptor = json!({ "schemaVersion": "3", "kind": "workload" });
        let annotations = json!({ DESCRIPTOR_ANNOTATION: descriptor.to_string() });
        let config = json!({ "config": { "Entrypoint": ["bash"], "Cmd": ["-l"] } });
        let read = facts("kit", &export(&annotations, &config)).unwrap();
        assert_eq!(read.digest, "sha256:aa");
        assert_eq!(read.descriptor, descriptor);
        assert_eq!(
            (read.entrypoint, read.cmd),
            (vec!["bash".to_owned()], vec!["-l".to_owned()])
        );

        let refusal =
            |files: &BTreeMap<String, Vec<u8>>| facts("kit", files).unwrap_err().to_string();
        assert!(refusal(&export(&json!({}), &config)).contains("not a kit"));
        let not_json = json!({ DESCRIPTOR_ANNOTATION: "{" });
        assert!(
            refusal(&export(&not_json, &config)).contains("descriptor of the image is not JSON")
        );
        let mut files = export(&annotations, &config);
        files.insert("manifest.json".to_owned(), b"[]".to_vec());
        assert!(refusal(&files).contains("names no configuration"));
        files.insert("index.json".to_owned(), b"{}".to_vec());
        assert!(refusal(&files).contains("names no image"));
        files.insert("index.json".to_owned(), b"[".to_vec());
        assert!(refusal(&files).contains("index.json of the image is not JSON"));
        assert!(refusal(&BTreeMap::new()).contains("holds no index.json"));
    }
}
