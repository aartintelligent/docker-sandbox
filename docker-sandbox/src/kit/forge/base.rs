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
//! The descriptor of a published kit is forwarded as it is, with capabilities
//! this crate does not know; the artifact is derived from typed views of the
//! parts it needs. The selection knows one rule: `agent-context@1` is left out
//! when the kit also declares `agent-context@2`, the only conflict published
//! kits hold.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::artifact::{
    BaseManifest, Credential, Environment, HubPublication, KitArtifact, KitUsage, Manifest,
    ManifestKind, SkillsRequest, capabilities, caps, commands, effective, is, selection,
};
use super::descriptor::{LIFECYCLE, LifecycleConfig, NETWORK, NetworkConfig, SchemaVersion};
use super::oci::{DESCRIPTOR_ANNOTATION, ImageConfig, ImageIndex};
use super::{args, export};
use crate::client::Client;
use crate::error::{Error, Result};

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
    pub(super) artifact: KitArtifact,
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

/// An entry of the `manifest.json` of an export, in the Docker save format.
#[derive(Debug, Deserialize)]
struct SavedImage {
    /// Path of the configuration of the image in the export.
    #[serde(rename = "Config")]
    config: String,
}

/// The fields of a published descriptor the artifact of its kit is derived
/// from.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Published {
    /// Version of the descriptor format, `3`.
    #[serde(default)]
    schema_version: Option<String>,
    /// Kind of kit, `workload` for a base kit.
    #[serde(default)]
    kind: Option<String>,
    /// Name shown for the kit.
    #[serde(default)]
    display_name: Option<String>,
    /// What the kit is for.
    #[serde(default)]
    description: Option<String>,
    /// Where the kit is defined.
    #[serde(default)]
    source_url: Option<String>,
    /// Licenses of the kit, kept as written.
    #[serde(default)]
    licenses: Option<Value>,
}

/// The fields of a capability of a published kit the artifact is derived
/// from.
#[derive(Debug, Deserialize)]
struct CapabilityView {
    /// Type of the capability, e.g. `com.docker.sandbox/credential@1`.
    #[serde(rename = "type", default)]
    kind: String,
    /// Whether the kit works without it.
    #[serde(default)]
    optional: bool,
    /// What it does.
    #[serde(default)]
    description: Option<String>,
    /// Its configuration, read according to its type.
    #[serde(default)]
    config: Value,
}

/// The configuration of an `agent-context` capability.
#[derive(Debug, Default, Deserialize)]
struct ContextConfig {
    /// Content of the context file.
    #[serde(default)]
    content: Option<String>,
    /// Name of the context file, e.g. `AGENTS.md`.
    #[serde(default)]
    filename: Option<String>,
}

/// The configuration of a `credential@1` capability.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialConfig {
    /// Service of the credential.
    #[serde(default)]
    service: Option<String>,
    /// How an API key is injected.
    #[serde(default)]
    api_key: Option<Value>,
    /// How an OAuth token is obtained.
    #[serde(default)]
    oauth: Option<Value>,
}

/// The configuration of an `agent-skills@1` capability.
#[derive(Debug, Default, Deserialize)]
struct SkillsConfig {
    /// Directory of the skills inside the sandbox.
    #[serde(default)]
    path: Option<String>,
}

/// Pulls the image of `reference`, exports it and rebuilds its kit artifact
/// with the argument values `args`.
///
/// # Errors
///
/// Returns the errors of the pull and export requests; [`Error::Archive`]
/// when the export is not a tar archive; [`Error::InvalidKit`] when the image
/// is not a v3 workload kit, holds parts this crate cannot read, or refuses
/// the arguments; [`Error::Forge`] when a pinned reference exports another
/// digest.
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

/// An [`Error::InvalidKit`] for `kit`.
fn invalid(kit: &str, message: String) -> Error {
    Error::InvalidKit {
        kit: kit.to_owned(),
        message,
    }
}

/// `value`, a part `what` of the kit `kit`, read as `T`.
fn read<T: DeserializeOwned>(kit: &str, what: &str, value: &Value) -> Result<T> {
    serde_json::from_value(value.clone())
        .map_err(|error| invalid(kit, format!("{what} is not one this crate reads: {error}")))
}

/// The file `path` of the export `files` of the image of `kit`, read as `T`.
fn export_file<T: DeserializeOwned>(
    kit: &str,
    files: &BTreeMap<String, Vec<u8>>,
    path: &str,
) -> Result<T> {
    let bytes = files
        .get(path)
        .ok_or_else(|| invalid(kit, format!("the export of the image holds no {path}")))?;
    serde_json::from_slice(bytes).map_err(|error| {
        invalid(
            kit,
            format!("{path} of the image is not one this crate reads: {error}"),
        )
    })
}

/// The digest, descriptor, entrypoint and command of the image of `kit`,
/// from the small files of its export.
fn facts(kit: &str, files: &BTreeMap<String, Vec<u8>>) -> Result<ImageFacts> {
    let index: ImageIndex = export_file(kit, files, "index.json")?;
    let digest = index
        .manifests
        .into_iter()
        .next()
        .map(|manifest| manifest.digest)
        .ok_or_else(|| invalid(kit, "the index of the image names no image".to_owned()))?;
    let blob = format!("blobs/sha256/{}", digest.trim_start_matches("sha256:"));
    let top: ImageIndex = export_file(kit, files, &blob)?;
    let encoded = top.annotations.get(DESCRIPTOR_ANNOTATION).ok_or_else(|| {
        invalid(
            kit,
            "the image is not a kit: it carries no kit descriptor".to_owned(),
        )
    })?;
    let descriptor: Value = serde_json::from_str(encoded).map_err(|error| {
        invalid(
            kit,
            format!("the kit descriptor of the image is not JSON: {error}"),
        )
    })?;
    let saved: Vec<SavedImage> = export_file(kit, files, "manifest.json")?;
    let config_path = saved
        .into_iter()
        .next()
        .map(|image| image.config)
        .ok_or_else(|| {
            invalid(
                kit,
                "the export of the image names no configuration".to_owned(),
            )
        })?;
    let config: ImageConfig = export_file(kit, files, &config_path)?;
    Ok(ImageFacts {
        digest,
        descriptor,
        entrypoint: config.config.entrypoint.unwrap_or_default(),
        cmd: config.config.cmd.unwrap_or_default(),
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
    let kit = reference.repository.as_str();
    let published: Published = read(kit, "the descriptor", descriptor)?;
    if published.schema_version.as_deref() != Some("3") {
        return Err(invalid(kit, "only v3 kits are rebuilt".to_owned()));
    }
    if published.kind.as_deref() != Some("workload") {
        return Err(invalid(kit, "a base kit is a workload kit".to_owned()));
    }
    let name = reference.name().to_owned();
    let pinned = format!("{kit}@{}", facts.digest);
    let source = format!("{kit}:{}", reference.tag);
    let resolved = args::resolve(kit, descriptor, args)?;
    let substituted = args::substitute(kit, descriptor, &resolved.values)?;

    let listed: Vec<Value> = capabilities(&substituted).cloned().collect();
    let views: Vec<CapabilityView> = listed
        .iter()
        .map(|capability| read(kit, "a capability", capability))
        .collect::<Result<_>>()?;
    let has_v2 = views.iter().any(|view| view.kind == CONTEXT_V2);
    let rejection = |capability: &Value| {
        (has_v2 && is(capability, CONTEXT_V1)).then(|| {
            format!("{CONTEXT_V1} is not selected: this kit set uses {CONTEXT_V2}, and the two cannot compose")
        })
    };
    let kept: Vec<(&Value, &CapabilityView)> = listed
        .iter()
        .zip(&views)
        .filter(|(capability, _)| rejection(capability).is_none())
        .collect();
    let mut selected = substituted.clone();
    selected["capabilities"] = kept
        .iter()
        .map(|(capability, _)| (*capability).clone())
        .collect();

    let context: ContextConfig = match kept
        .iter()
        .find(|(_, view)| view.kind == CONTEXT_V1 || view.kind == CONTEXT_V2)
    {
        Some((_, view)) => config(kit, view)?,
        None => ContextConfig::default(),
    };
    let lifecycles: Vec<LifecycleConfig> = of_kind(kit, &kept, LIFECYCLE)?;
    let policies: Vec<NetworkConfig> = of_kind(kit, &kept, NETWORK)?;
    let mut artifact = KitArtifact {
        manifest: manifest(&published, &name, &pinned, facts, &context),
        v3: descriptor.clone(),
        v3_effective: with_key_names(effective(&selected)),
        v3_selection: selection(&listed, &source, rejection),
        v3_digest: facts.digest.clone(),
        v3_reference: pinned.clone(),
        v3_entrypoint: Some(facts.entrypoint.clone()),
        storage_source: Some(format!("oci://{source}")),
        kit_usage: KitUsage::published(),
        kit_hub_publication: kit.strip_prefix("docker.io/").map(|hub| HubPublication {
            digest: facts.digest.clone(),
            repository: hub.to_owned(),
        }),
        licenses: published.licenses,
        agent_context: context.content,
        commands: commands(&lifecycles),
        caps: caps(&policies),
        args: resolved.declared,
        v3_arg_values: resolved.values,
        environment: Environment {
            variables: resolved.environment,
        },
        ..KitArtifact::default()
    };
    insert_declared_access(&mut artifact, kit, &kept)?;
    Ok(BaseKit {
        reference: pinned,
        name,
        artifact,
    })
}

/// The configuration of the capability `view` of `kit`, read as `T`; the
/// default of `T` when it has none.
fn config<T: DeserializeOwned + Default>(kit: &str, view: &CapabilityView) -> Result<T> {
    if view.config.is_null() {
        return Ok(T::default());
    }
    read(
        kit,
        &format!("the configuration of {}", view.kind),
        &view.config,
    )
}

/// The configurations of the capabilities of `kept` of the type `kind`.
fn of_kind<T: DeserializeOwned + Default>(
    kit: &str,
    kept: &[(&Value, &CapabilityView)],
    kind: &str,
) -> Result<Vec<T>> {
    kept.iter()
        .filter(|(_, view)| view.kind == kind)
        .map(|(_, view)| config(kit, view))
        .collect()
}

/// `effective` with the API keys of its capabilities named, `""` when the kit
/// names none, as the CLI writes them.
fn with_key_names(mut effective: Value) -> Value {
    let capabilities = effective
        .get_mut("capabilities")
        .and_then(Value::as_array_mut);
    for capability in capabilities.into_iter().flatten() {
        if let Some(api_key) = capability.pointer_mut("/config/apiKey") {
            *api_key = with_name(api_key.take());
        }
    }
    effective
}

/// `api_key` named `""` when it names nothing.
fn with_name(mut api_key: Value) -> Value {
    if let Some(fields) = api_key.as_object_mut() {
        fields.entry("name").or_insert_with(|| json!(""));
    }
    api_key
}

/// The `manifest` of a base kit artifact: the identity of the kit, the program
/// of its agent and the options it starts with (the entrypoint and command of
/// its image), and the file its selected agent `context` goes to.
fn manifest(
    published: &Published,
    name: &str,
    pinned: &str,
    facts: &ImageFacts,
    context: &ContextConfig,
) -> Manifest {
    let (binary, run_options) = match facts.entrypoint.split_first() {
        Some((binary, options)) => (
            Some(binary.clone()),
            Some(options.iter().chain(&facts.cmd).cloned().collect()),
        ),
        None => (None, None),
    };
    Manifest {
        description: published.description.clone(),
        display_name: published.display_name.clone(),
        kind: ManifestKind::Sandbox,
        name: name.to_owned(),
        schema_version: SchemaVersion::V3,
        template: pinned.to_owned(),
        base: Some(BaseManifest {
            source_url: published.source_url.clone(),
            binary,
            run_options,
            ai_filename: context.filename.clone(),
        }),
    }
}

/// Fills `artifact` with the credentials and the skills directories the
/// selected capabilities `kept` of `kit` declare, as the CLI summarizes them.
/// A credential is a declaration only: no value goes into the package.
fn insert_declared_access(
    artifact: &mut KitArtifact,
    kit: &str,
    kept: &[(&Value, &CapabilityView)],
) -> Result<()> {
    for (_, view) in kept {
        if view.kind == CREDENTIAL {
            let config: CredentialConfig = config(kit, view)?;
            if view.optional
                && let Some(service) = &config.service
            {
                artifact.optional_credentials.push(service.clone());
            }
            artifact.credentials.push(Credential {
                service: config.service,
                description: view.description.clone(),
                api_key: config.api_key.map(with_name),
                oauth: config.oauth,
            });
        } else if view.kind == SKILLS {
            let config: SkillsConfig = config(kit, view)?;
            artifact.skills_requests.push(SkillsRequest {
                mode: SKILLS_MODE,
                optional: view.optional,
                path: config.path,
            });
        }
    }
    Ok(())
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
        let artifact = &kit.artifact.into_json();
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
        let artifact = &kit.artifact.into_json();
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
        let artifact = &kit.artifact.into_json();
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
        let artifact = &kit.artifact.into_json();
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

    #[test]
    fn capabilities_without_configuration_take_its_defaults() {
        let mut facts = shell();
        facts.descriptor["capabilities"] = json!([{ "type": CREDENTIAL }, { "type": SKILLS }]);
        let kit = base_kit(&reference("sbx/shell"), &facts, &BTreeMap::new()).unwrap();
        let artifact = kit.artifact.into_json();
        assert_eq!(artifact["credentials"], json!([{ "service": null }]));
        assert_eq!(
            artifact["skillsRequests"],
            json!([{ "mode": "readonly", "optional": false, "path": null }])
        );
        assert!(artifact.get("optionalCredentials").is_none());
    }

    #[test]
    fn capabilities_this_crate_cannot_read_are_errors() {
        let refusal = |capability: Value| {
            let mut facts = shell();
            facts.descriptor["capabilities"] = json!([capability]);
            let error = base_kit(&reference("sbx/shell"), &facts, &BTreeMap::new()).unwrap_err();
            assert!(matches!(error, Error::InvalidKit { .. }), "{error}");
            error.to_string()
        };
        assert!(refusal(json!("sbx")).contains("a capability is not one this crate reads"));
        let credential = json!({ "type": CREDENTIAL, "config": { "service": 3 } });
        assert!(
            refusal(credential)
                .contains("the configuration of com.docker.sandbox/credential@1 is not one")
        );
        let mut facts = shell();
        facts.descriptor = json!({ "schemaVersion": 3 });
        let error = base_kit(&reference("sbx/shell"), &facts, &BTreeMap::new()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the descriptor is not one this crate reads"),
            "{error}"
        );
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
        assert!(refusal(&files).contains("index.json of the image is not one this crate reads"));
        assert!(refusal(&BTreeMap::new()).contains("holds no index.json"));
    }
}
