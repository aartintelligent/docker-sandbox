//! `kit-forge`: writes the package of a v3 mixin without the `sbx` CLI, to
//! measure what `sandboxd` needs from a kit.
//!
//! The CLI turns a kit source into two things the daemon takes: an OCI image
//! layout holding the descriptor, loaded with `POST /docker/images/load` under
//! `sbx-kit-src:<stem>-<hash>`, and a kit artifact in the creation request.
//! This command writes both from the kit directory alone, and takes the
//! artifact of the base kit from a package compiled before, so that only the
//! mixin is forged. The output has the layout of a `KitPackage`:
//!
//! ```text
//! <out>/kit.json            metadata: forged, base package, omitted fields, image
//! <out>/create-body.json    creation request template, "name": "{{name}}"
//! <out>/images/00.oci.tar   source image of the mixin
//! ```
//!
//! It is an experiment of the audit, not a resolver: the descriptor is read as
//! JSON (a `spec.yaml` written in the JSON subset of YAML, after its comment
//! lines), only declarative mixins are handled (`lifecycle@1` commands and
//! `network-policy@1` runtime rules), `--omit` drops artifact fields to find the
//! ones the daemon requires, and `--compare` prints how the forged artifact
//! differs from the one the CLI wrote for the same directory.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Directory of the descriptor inside the layer of a source image.
const KIT_DIR: &str = "usr/share/sandbox/kit";

/// Repository of the source images, as the CLI names them.
const SOURCE_REPOSITORY: &str = "sbx-kit-src";

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

/// Options of the `kit-forge` command.
pub(crate) struct ForgeOpts<'a> {
    /// Kit directory holding `spec.yaml`.
    pub kit: &'a Path,
    /// Package compiled by the CLI whose base kit is reused.
    pub base_package: &'a Path,
    /// Output directory of the package, replaced when it exists.
    pub out: &'a Path,
    /// Top-level fields left out of the mixin artifact.
    pub omit: &'a [String],
    /// Package compiled by the CLI for the same kit, to compare with.
    pub compare: Option<&'a Path>,
}

/// The source image of a mixin, as an OCI image layout archive.
struct SourceImage {
    /// The archive, loaded with `POST /docker/images/load`.
    archive: Vec<u8>,
    /// Reference the daemon stores it under, e.g. `sbx-kit-src:spec-0123abcd4567`.
    reference: String,
    /// Digest of its manifest, `sha256:…`.
    manifest_digest: String,
}

/// Writes the package of the mixin of `opts.kit`, then compares it when asked.
pub(crate) fn run(opts: &ForgeOpts<'_>) -> Result<()> {
    let kit_dir = opts
        .kit
        .canonicalize()
        .with_context(|| format!("kit directory not found: {}", opts.kit.display()))?;
    let name = kit_dir
        .file_name()
        .context("the kit directory has no name")?
        .to_string_lossy()
        .into_owned();
    let reference_path = kit_dir.display().to_string();
    let (text, descriptor) = read_descriptor(&kit_dir.join("spec.yaml"))?;

    let image = source_image("spec", &text, &descriptor)?;
    let mut artifact = mixin_artifact(
        &name,
        &reference_path,
        &descriptor,
        &image.reference,
        &image.manifest_digest,
    );
    if let Some(fields) = artifact.as_object_mut() {
        for field in opts.omit {
            fields.remove(field);
        }
    }

    let base = read_json(&opts.base_package.join("create-body.json"))?;
    let template = compose(&base, &reference_path, artifact.clone())?;
    write_package(opts, &name, &template, &image)?;
    eprintln!(
        "[kit-forge] package written to {} (mixin {name}, image {}, {} field(s) omitted)",
        opts.out.display(),
        image.reference,
        opts.omit.len()
    );

    if let Some(other) = opts.compare {
        compare(&artifact, &name, other)?;
    }
    Ok(())
}

/// The text of `spec.yaml` and its descriptor, read as JSON after the comment
/// lines that open it.
fn read_descriptor(path: &Path) -> Result<(String, Value)> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    let body: String = text
        .lines()
        .skip_while(|line| line.trim_start().starts_with('#') || line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let descriptor: Value = serde_json::from_str(&body).with_context(|| {
        format!(
            "{}: kit-forge reads descriptors written in the JSON subset of YAML",
            path.display()
        )
    })?;
    if descriptor.get("schemaVersion").and_then(Value::as_str) != Some("3") {
        bail!("{}: only v3 descriptors are forged", path.display());
    }
    if descriptor.get("kind").and_then(Value::as_str) != Some("mixin") {
        bail!("{}: only mixins are forged", path.display());
    }
    Ok((text, descriptor))
}

/// The capabilities of `descriptor`, empty when it declares none.
fn capabilities(descriptor: &Value) -> &[Value] {
    descriptor
        .get("capabilities")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Builds the OCI image layout of the descriptor, as the CLI does for a local
/// kit: one layer holding `<KIT_DIR>/<stem>/kit.yaml`, the descriptor in the
/// annotations of the manifest, every entry owned by root and dated 1970.
fn source_image(stem: &str, text: &str, descriptor: &Value) -> Result<SourceImage> {
    let layer = descriptor_layer(stem, text)?;
    let layer_digest = digest(&layer);

    let architecture = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    let config = serde_json::to_vec(&json!({
        "architecture": architecture,
        "os": "linux",
        "config": {},
        "rootfs": { "type": "layers", "diff_ids": [layer_digest] },
    }))?;
    let config_digest = digest(&config);

    let encoded = serde_json::to_string(descriptor)?;
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": layer_digest,
            "size": layer.len(),
        }],
        "annotations": annotations(descriptor, &encoded),
    }))?;
    let manifest_digest = digest(&manifest);

    let hash = digest(encoded.as_bytes());
    let short = hash
        .trim_start_matches("sha256:")
        .get(..12)
        .unwrap_or_default();
    let reference = format!("{SOURCE_REPOSITORY}:{stem}-{short}");
    let index = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": manifest_digest,
            "size": manifest.len(),
            "annotations": {
                "io.containerd.image.name": format!("docker.io/library/{reference}"),
                "org.opencontainers.image.ref.name": reference,
            },
        }],
    }))?;

    // The CLI writes the blobs in digest order, then the index and the layout
    // marker.
    let mut blobs = [
        (layer_digest, layer),
        (config_digest, config),
        (manifest_digest.clone(), manifest),
    ];
    blobs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut archive = tar::Builder::new(Vec::new());
    for (blob_digest, bytes) in &blobs {
        let path = format!("blobs/sha256/{}", blob_digest.trim_start_matches("sha256:"));
        append(&mut archive, &path, tar::EntryType::Regular, 0o644, bytes)?;
    }
    append(
        &mut archive,
        "index.json",
        tar::EntryType::Regular,
        0o644,
        &index,
    )?;
    append(
        &mut archive,
        "oci-layout",
        tar::EntryType::Regular,
        0o644,
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;
    Ok(SourceImage {
        archive: archive.into_inner()?,
        reference,
        manifest_digest,
    })
}

/// The layer of a source image: the directories of `<KIT_DIR>/<stem>`, then
/// `kit.yaml` holding `text`.
fn descriptor_layer(stem: &str, text: &str) -> Result<Vec<u8>> {
    let mut layer = tar::Builder::new(Vec::new());
    let mut dir = String::new();
    for part in KIT_DIR.split('/').chain([stem]) {
        if !dir.is_empty() {
            dir.push('/');
        }
        dir.push_str(part);
        append(&mut layer, &dir, tar::EntryType::Directory, 0o755, &[])?;
    }
    append(
        &mut layer,
        &format!("{dir}/kit.yaml"),
        tar::EntryType::Regular,
        0o644,
        text.as_bytes(),
    )?;
    Ok(layer.into_inner()?)
}

/// The annotations of the manifest of a source image: the OCI title,
/// description and version, and the descriptor `encoded` as JSON with its
/// capability types.
fn annotations(descriptor: &Value, encoded: &str) -> Value {
    let types: Vec<&str> = capabilities(descriptor)
        .iter()
        .filter_map(|c| c.get("type").and_then(Value::as_str))
        .collect();
    let text_field = |key: &str| descriptor.get(key).cloned().unwrap_or(Value::Null);
    json!({
        "org.opencontainers.image.description": text_field("description"),
        "org.opencontainers.image.title": text_field("displayName"),
        "org.opencontainers.image.version": text_field("version"),
        "vnd.docker.sandbox.kit.built-by": json!({
            "name": "docker-sandbox/audit",
            "version": env!("CARGO_PKG_VERSION"),
        }).to_string(),
        "vnd.docker.sandbox.kit.capabilities": types.join(","),
        "vnd.docker.sandbox.kit.descriptor": encoded,
        "vnd.docker.sandbox.kit.schema-version": "3",
    })
}

/// Appends one entry owned by root and dated 1970 to `builder`.
fn append(
    builder: &mut tar::Builder<Vec<u8>>,
    path: &str,
    kind: tar::EntryType,
    mode: u32,
    data: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(kind);
    header.set_mode(mode);
    header.set_size(data.len() as u64);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    builder
        .append_data(&mut header, path, data)
        .with_context(|| format!("archiving {path}"))
}

/// `sha256:<hex>` of `bytes`.
fn digest(bytes: &[u8]) -> String {
    let mut hex = String::from("sha256:");
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The kit artifact of the mixin, with the fields the CLI writes for a
/// declarative local mixin.
fn mixin_artifact(
    name: &str,
    reference_path: &str,
    descriptor: &Value,
    reference: &str,
    manifest_digest: &str,
) -> Value {
    let text_field = |key: &str| descriptor.get(key).cloned().unwrap_or(Value::Null);
    let mut artifact = Map::new();
    artifact.insert(
        "manifest".into(),
        json!({
            "description": text_field("description"),
            "displayName": text_field("displayName"),
            "kind": "mixin",
            "name": name,
            "schemaVersion": "3",
            "template": reference,
        }),
    );
    artifact.insert("v3".into(), descriptor.clone());
    artifact.insert("v3Effective".into(), effective(descriptor));
    artifact.insert("v3Selection".into(), selection(descriptor, reference_path));
    artifact.insert("v3Digest".into(), json!(manifest_digest));
    artifact.insert("v3Reference".into(), json!(reference_path));
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

/// The `lifecycle@1` command lists of `capability`, by phase.
fn lifecycle_phases(capability: &Value) -> impl Iterator<Item = (&str, &Vec<Value>)> {
    let is_lifecycle =
        capability.get("type").and_then(Value::as_str) == Some("com.docker.sandbox/lifecycle@1");
    ["install", "startup"].into_iter().filter_map(move |phase| {
        is_lifecycle
            .then(|| capability.pointer(&format!("/config/{phase}")))
            .flatten()
            .and_then(Value::as_array)
            .map(|steps| (phase, steps))
    })
}

/// `descriptor` normalized as the CLI does: lifecycle commands written as
/// `["sh", "-c", command]`.
fn effective(descriptor: &Value) -> Value {
    let mut effective = descriptor.clone();
    if let Some(capabilities) = effective
        .get_mut("capabilities")
        .and_then(Value::as_array_mut)
    {
        for capability in capabilities {
            for phase in ["install", "startup"] {
                let Some(steps) = capability
                    .pointer_mut(&format!("/config/{phase}"))
                    .and_then(Value::as_array_mut)
                else {
                    continue;
                };
                for step in steps {
                    if let Some(command) = step.get("command").and_then(Value::as_str) {
                        let command = json!(["sh", "-c", command]);
                        step["command"] = command;
                    }
                }
            }
        }
    }
    effective
}

/// The capability selection of a mixin whose capabilities are all accepted.
fn selection(descriptor: &Value, reference_path: &str) -> Value {
    let mut capabilities = Vec::new();
    let mut selected = Vec::new();
    for (index, capability) in capabilities_of(descriptor).enumerate() {
        let path = format!("capabilities[{index}]");
        let source = json!({ "kit": reference_path, "path": path });
        let mut with_source = capability.clone();
        with_source["source"] = source.clone();
        capabilities.push(with_source);
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
    json!({ "capabilities": capabilities, "selected": selected })
}

/// The capabilities of `descriptor`, in order.
fn capabilities_of(descriptor: &Value) -> impl Iterator<Item = &Value> {
    capabilities(descriptor).iter()
}

/// The commands of the artifact: install commands wrapped in a cleared
/// environment that keeps [`INSTALL_ENV`] and the variables the step names,
/// startup commands as `["sh", "-c", command]`. `None` without lifecycle.
fn commands(descriptor: &Value) -> Option<Value> {
    let mut phases = Map::new();
    for capability in capabilities_of(descriptor) {
        for (phase, steps) in lifecycle_phases(capability) {
            let list = phases
                .entry(phase)
                .or_insert_with(|| Value::Array(Vec::new()));
            for step in steps {
                let Some(command) = step.get("command").and_then(Value::as_str) else {
                    continue;
                };
                let mut entry = Map::new();
                let wrapped = if phase == "install" {
                    json!(wrap_install(command, step))
                } else {
                    json!(["sh", "-c", command])
                };
                entry.insert("command".into(), wrapped);
                for key in ["description", "user", "background"] {
                    if let Some(value) = step.get(key) {
                        entry.insert(key.into(), value.clone());
                    }
                }
                if let Some(list) = list.as_array_mut() {
                    list.push(Value::Object(entry));
                }
            }
        }
    }
    (!phases.is_empty()).then_some(Value::Object(phases))
}

/// `env -i NAME="$NAME" … sh -c '<command>'`, the form the CLI gives install
/// commands.
fn wrap_install(command: &str, step: &Value) -> String {
    let extra = step
        .get("env")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    let mut wrapped = String::from("env -i");
    for name in INSTALL_ENV.iter().copied().chain(extra) {
        let _ = write!(wrapped, " {name}=\"${name}\"");
    }
    let _ = write!(wrapped, " sh -c '{}'", command.replace('\'', r"'\''"));
    wrapped
}

/// The runtime network rules of the `network-policy@1` capabilities, as the
/// `caps.network` of the artifact. `None` without any.
fn network(descriptor: &Value) -> Option<Value> {
    let mut rules = Map::new();
    for capability in capabilities_of(descriptor) {
        if capability.get("type").and_then(Value::as_str)
            != Some("com.docker.sandbox/network-policy@1")
        {
            continue;
        }
        for decision in ["allow", "deny"] {
            let Some(hosts) = capability
                .pointer(&format!("/config/runtime/{decision}"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            if let Some(list) = rules
                .entry(decision)
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
            {
                list.extend(hosts.iter().cloned());
            }
        }
    }
    (!rules.is_empty()).then_some(Value::Object(rules))
}

/// The creation template of `base` with the mixin appended: its base kit, then
/// the forged artifact.
fn compose(base: &Value, reference_path: &str, artifact: Value) -> Result<Value> {
    let base_ref = base
        .pointer("/kits/0")
        .cloned()
        .context("the base package has no kit reference")?;
    let base_artifact = base
        .pointer("/kit_artifacts/0")
        .cloned()
        .context("the base package has no kit artifact")?;
    let mut template = base.clone();
    template["name"] = json!("{{name}}");
    template["kits"] = json!([base_ref, reference_path]);
    template["kit_artifacts"] = Value::Array(vec![base_artifact, artifact]);
    Ok(template)
}

/// Writes the package of `template` and `image` into `opts.out`.
fn write_package(
    opts: &ForgeOpts<'_>,
    name: &str,
    template: &Value,
    image: &SourceImage,
) -> Result<()> {
    let _ = std::fs::remove_dir_all(opts.out);
    std::fs::create_dir_all(opts.out.join("images"))
        .with_context(|| opts.out.display().to_string())?;
    let file = "images/00.oci.tar";
    std::fs::write(opts.out.join(file), &image.archive)?;
    let metadata = json!({
        "forged_by": { "name": "docker-sandbox/audit", "version": env!("CARGO_PKG_VERSION") },
        "base_package": opts.base_package.display().to_string(),
        "mixins": [name],
        "omitted": opts.omit,
        "images": [{
            "file": file,
            "ref": image.reference,
            "manifest_digest": image.manifest_digest,
            "bytes": image.archive.len(),
        }],
    });
    for (file, value) in [("create-body.json", template), ("kit.json", &metadata)] {
        std::fs::write(opts.out.join(file), serde_json::to_vec_pretty(value)?)?;
    }
    Ok(())
}

/// Prints how the forged `artifact` of `name` differs from the artifact of the
/// same mixin in the package `other`, top-level field by field.
fn compare(artifact: &Value, name: &str, other: &Path) -> Result<()> {
    let body = read_json(&other.join("create-body.json"))?;
    let theirs = body
        .get("kit_artifacts")
        .and_then(Value::as_array)
        .and_then(|artifacts| {
            artifacts
                .iter()
                .find(|a| a.pointer("/manifest/name").and_then(Value::as_str) == Some(name))
        })
        .with_context(|| format!("{}: no artifact of {name}", other.display()))?;
    let empty = Map::new();
    let ours = artifact.as_object().unwrap_or(&empty);
    let theirs = theirs.as_object().unwrap_or(&empty);
    let mut keys: Vec<&String> = ours.keys().chain(theirs.keys()).collect();
    keys.sort();
    keys.dedup();
    println!(
        "[kit-forge] forged artifact of {name} against {}:",
        other.display()
    );
    for key in keys {
        let line = match (ours.get(key), theirs.get(key)) {
            (Some(a), Some(b)) => {
                let mut paths = Vec::new();
                differences(a, b, &format!("/{key}"), &mut paths);
                if paths.is_empty() {
                    "same".to_owned()
                } else {
                    format!("differs at {}", paths.join(", "))
                }
            }
            (Some(_), None) => "forged only".to_owned(),
            (None, Some(_)) => "sbx only".to_owned(),
            (None, None) => continue,
        };
        println!("  {key:<12} {line}");
    }
    Ok(())
}

/// Collects into `paths` the JSON pointers where `ours` and `theirs` differ, a
/// differing scalar or array length ending the walk at its pointer.
fn differences(ours: &Value, theirs: &Value, at: &str, paths: &mut Vec<String>) {
    match (ours, theirs) {
        (Value::Object(our_fields), Value::Object(their_fields)) => {
            let mut keys: Vec<&String> = our_fields.keys().chain(their_fields.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let path = format!("{at}/{key}");
                match (our_fields.get(key), their_fields.get(key)) {
                    (Some(mine), Some(other)) => differences(mine, other, &path, paths),
                    _ => paths.push(path),
                }
            }
        }
        (Value::Array(our_items), Value::Array(their_items))
            if our_items.len() == their_items.len() =>
        {
            for (index, (mine, other)) in our_items.iter().zip(their_items).enumerate() {
                differences(mine, other, &format!("{at}/{index}"), paths);
            }
        }
        _ if ours == theirs => {}
        _ => paths.push(at.to_owned()),
    }
}

/// Reads a JSON file.
fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    serde_json::from_str(&text).with_context(|| format!("{}: invalid JSON", path.display()))
}

/// Default output directory of a forged package for the kit `kit`.
pub(crate) fn default_out(output: &Path, kit: &Path) -> PathBuf {
    let name = kit
        .file_name()
        .map_or_else(|| "kit".to_owned(), |n| n.to_string_lossy().into_owned());
    output.join("kits").join(format!("{name}-forged"))
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    /// A mixin with an install command, a startup command and network rules.
    fn mixin() -> Value {
        json!({
            "schemaVersion": "3",
            "kind": "mixin",
            "displayName": "Test mixin",
            "description": "A test",
            "version": "0.1.0",
            "capabilities": [
                { "type": "com.docker.sandbox/network-policy@1",
                  "config": { "runtime": { "allow": ["a.example.com:443"], "deny": ["b.example.com"] } } },
                { "type": "com.docker.sandbox/lifecycle@1",
                  "config": {
                    "install": [{ "command": "echo 'hi' > /tmp/x", "user": "agent", "env": ["EXTRA"] }],
                    "startup": [{ "command": "date", "user": "agent", "background": true }]
                  } }
            ]
        })
    }

    #[test]
    fn digests_are_sha256_in_hexadecimal() {
        assert_eq!(
            digest(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn install_commands_are_wrapped_as_the_cli_does() {
        let step = json!({ "env": ["EXTRA"] });
        let wrapped = wrap_install("echo 'hi'", &step);
        assert!(wrapped.starts_with("env -i PATH=\"$PATH\" HOME=\"$HOME\""));
        assert!(wrapped.ends_with(r#"EXTRA="$EXTRA" sh -c 'echo '\''hi'\'''"#));
    }

    #[test]
    fn the_artifact_holds_the_fields_of_the_cli() {
        let artifact = mixin_artifact(
            "test-mixin",
            "/kits/test-mixin",
            &mixin(),
            "sbx-kit-src:spec-0123",
            "sha256:00",
        );
        assert_eq!(artifact["manifest"]["template"], "sbx-kit-src:spec-0123");
        assert_eq!(
            artifact["caps"]["network"],
            json!({ "allow": ["a.example.com:443"], "deny": ["b.example.com"] })
        );
        assert_eq!(
            artifact["commands"]["startup"][0]["command"],
            json!(["sh", "-c", "date"])
        );
        assert_eq!(artifact["commands"]["startup"][0]["background"], true);
        assert_eq!(
            artifact["v3Effective"]["capabilities"][1]["config"]["install"][0]["command"],
            json!(["sh", "-c", "echo 'hi' > /tmp/x"])
        );
        assert_eq!(
            artifact["v3Selection"]["selected"][1]["path"],
            "capabilities[1]"
        );
        assert_eq!(
            artifact["v3Selection"]["capabilities"][0]["source"]["kit"],
            "/kits/test-mixin"
        );
    }

    #[test]
    fn the_source_image_is_a_consistent_oci_layout() {
        let image = source_image("spec", "{}", &mixin()).unwrap();
        let mut archive = tar::Archive::new(image.archive.as_slice());
        let mut files = std::collections::BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().display().to_string();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            files.insert(path, bytes);
        }
        let index: Value = serde_json::from_slice(&files["index.json"]).unwrap();
        assert_eq!(index["manifests"][0]["digest"], image.manifest_digest);
        assert_eq!(
            index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
            image.reference
        );
        for (path, bytes) in &files {
            if let Some(hex) = path.strip_prefix("blobs/sha256/") {
                assert_eq!(digest(bytes), format!("sha256:{hex}"), "{path}");
            }
        }
        assert!(image.reference.starts_with("sbx-kit-src:spec-"));
    }

    #[test]
    fn descriptors_are_read_after_their_comment_lines() {
        let dir = std::env::temp_dir().join(format!("audit-kit-forge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("spec.yaml");
        std::fs::write(&path, format!("# syntax=docker/sandbox-kit:3\n{}", mixin())).unwrap();
        let (_, descriptor) = read_descriptor(&path).unwrap();
        assert_eq!(descriptor["kind"], "mixin");
        std::fs::write(&path, "schemaVersion: \"3\"\n").unwrap();
        assert!(read_descriptor(&path).is_err(), "YAML proper is not read");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn differences_are_reported_as_pointers() {
        let mut paths = Vec::new();
        differences(
            &json!({ "a": 1, "b": [1, 2], "c": { "d": true } }),
            &json!({ "a": 1, "b": [1, 3], "c": {} }),
            "",
            &mut paths,
        );
        assert_eq!(paths, ["/b/1", "/c/d"]);
    }
}
