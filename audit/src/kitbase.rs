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
//! its capability selection from its descriptor.

use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Context, Result, bail};
use docker_sandbox::Client;
use serde_json::{Map, Value, json};

/// Blobs of the export kept in memory: the index, manifests and configuration
/// are a few kilobytes, layers are skipped.
const SMALL_BLOB: u64 = 1 << 20;

/// Agent context capability that cannot compose with [`CONTEXT_V2`].
const CONTEXT_V1: &str = "com.docker.sandbox/agent-context@1";

/// Agent context capability the CLI selects over [`CONTEXT_V1`].
const CONTEXT_V2: &str = "com.docker.sandbox/agent-context@2";

/// A base kit rebuilt from its image.
pub(crate) struct BaseKit {
    /// Reference pinned by digest, the first entry of `kits`, e.g.
    /// `docker.io/sbx/shell@sha256:…`.
    pub reference: String,
    /// Name of the kit, the `agent` of the creation request, e.g. `shell`.
    pub name: String,
    /// The kit artifact.
    pub artifact: Value,
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

/// Pulls `requested` (e.g. `docker.io/sbx/shell:latest`), exports it and
/// rebuilds its kit artifact.
pub(crate) async fn forge(client: &Client, requested: &str) -> Result<BaseKit> {
    client
        .images()
        .pull(requested)
        .await
        .with_context(|| format!("pulling {requested}"))?;
    let archive = client
        .images()
        .export(requested)
        .await
        .with_context(|| format!("exporting {requested}"))?;
    let facts = read_export(&archive)?;
    base_kit(requested, &facts)
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

/// `(repository, tag)` of a reference such as `docker.io/sbx/shell:latest`;
/// the tag defaults to `latest`.
fn split_reference(reference: &str) -> (&str, &str) {
    let last_slash = reference.rfind('/').unwrap_or(0);
    match reference.rfind(':') {
        Some(colon) if colon > last_slash => (&reference[..colon], &reference[colon + 1..]),
        _ => (reference, "latest"),
    }
}

/// The base kit of `requested`, from the facts its export gave.
fn base_kit(requested: &str, facts: &ImageFacts) -> Result<BaseKit> {
    let descriptor = &facts.descriptor;
    if descriptor.get("schemaVersion").and_then(Value::as_str) != Some("3") {
        bail!("{requested}: only v3 kits are rebuilt");
    }
    if descriptor.get("kind").and_then(Value::as_str) != Some("workload") {
        bail!("{requested}: not a workload kit");
    }
    let (repository, _) = split_reference(requested);
    let name = repository
        .rsplit('/')
        .next()
        .unwrap_or(repository)
        .to_owned();
    let pinned = format!("{repository}@{}", facts.digest);
    let source = if requested.contains(':') {
        requested.to_owned()
    } else {
        format!("{requested}:latest")
    };

    let capabilities = descriptor
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

    let mut effective = descriptor.clone();
    effective["capabilities"] = Value::Array(
        capabilities
            .iter()
            .filter(|c| !rejected(c))
            .cloned()
            .collect(),
    );

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

    let mut artifact = Map::new();
    artifact.insert("manifest".into(), manifest);
    artifact.insert("v3".into(), descriptor.clone());
    artifact.insert("v3Effective".into(), effective);
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
    Ok(BaseKit {
        reference: pinned,
        name,
        artifact: Value::Object(artifact),
    })
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

    #[test]
    fn references_are_split_into_repository_and_tag() {
        assert_eq!(
            split_reference("docker.io/sbx/shell:latest"),
            ("docker.io/sbx/shell", "latest")
        );
        assert_eq!(
            split_reference("docker.io/sbx/shell"),
            ("docker.io/sbx/shell", "latest")
        );
        assert_eq!(
            split_reference("localhost:5000/kits/tools"),
            ("localhost:5000/kits/tools", "latest")
        );
    }

    #[test]
    fn the_base_artifact_holds_the_fields_of_the_cli() {
        let kit = base_kit("docker.io/sbx/shell:latest", &shell()).unwrap();
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
    }

    #[test]
    fn only_v3_workloads_are_rebuilt() {
        let mut facts = shell();
        facts.descriptor["kind"] = json!("mixin");
        assert!(base_kit("docker.io/sbx/shell", &facts).is_err());
        facts.descriptor["schemaVersion"] = json!("2");
        assert!(base_kit("docker.io/sbx/shell", &facts).is_err());
    }
}
