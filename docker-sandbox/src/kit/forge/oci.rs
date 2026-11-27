//! The source image of a mixin: the OCI image layout the daemon loads
//! (`POST /docker/images/load`) and finds the mixin by.
//!
//! The layout is the one the `sbx` CLI writes for a local kit, measured by the
//! audit: one layer holding `usr/share/sandbox/kit/spec/kit.yaml`, the
//! descriptor as JSON in the annotations of the manifest, the image named
//! `sbx-kit-src:spec-<hash>`, every entry owned by root and dated 1970, so
//! that the same descriptor always gives the same archive. The daemon checks
//! neither the hash of the name nor the form of `kit.yaml`.

use std::fmt::Write as _;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Directory of the descriptor inside the layer.
const KIT_DIR: &str = "usr/share/sandbox/kit/spec";

/// Repository the daemon stores source images under.
const REPOSITORY: &str = "sbx-kit-src";

/// Content of the `oci-layout` file that marks an OCI image layout.
const LAYOUT_MARKER: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;

/// Operation named by the errors of this module.
const OPERATION: &str = "kit source image";

/// A source image, as an OCI image layout archive.
pub(super) struct SourceImage {
    /// The archive, for `POST /docker/images/load`.
    pub(super) archive: Vec<u8>,
    /// Name the daemon stores it under, e.g. `sbx-kit-src:spec-0123abcd4567`.
    pub(super) reference: String,
    /// Digest of its manifest, `sha256:…`.
    pub(super) manifest_digest: String,
}

/// The source image of the mixin `descriptor`, whose `kit.yaml` holds `spec`.
///
/// # Errors
///
/// Returns [`Error::Archive`] if the archive cannot be written.
pub(super) fn source_image(spec: &str, descriptor: &Value) -> Result<SourceImage> {
    let layer = layer(spec)?;
    let layer_digest = digest(&layer);
    let config = serde_json::to_vec(&json!({
        "architecture": architecture(std::env::consts::ARCH),
        "os": "linux",
        "config": {},
        "rootfs": { "type": "layers", "diff_ids": [layer_digest] },
    }))
    .map_err(archive_error)?;
    let config_digest = digest(&config);

    let encoded = descriptor.to_string();
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
    }))
    .map_err(archive_error)?;
    let manifest_digest = digest(&manifest);

    let hash = digest(encoded.as_bytes());
    let short = hash.get(7..19).unwrap_or_default();
    let reference = format!("{REPOSITORY}:spec-{short}");
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
    }))
    .map_err(archive_error)?;

    // The blobs in digest order, then the index and the layout marker, as
    // the CLI writes them.
    let mut blobs = [
        (layer_digest, layer),
        (config_digest, config),
        (manifest_digest.clone(), manifest),
    ];
    blobs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut archive = tar::Builder::new(Vec::new());
    for (blob_digest, bytes) in &blobs {
        let path = format!("blobs/sha256/{}", blob_digest.trim_start_matches("sha256:"));
        file(&mut archive, &path, bytes)?;
    }
    file(&mut archive, "index.json", &index)?;
    file(&mut archive, "oci-layout", LAYOUT_MARKER)?;
    Ok(SourceImage {
        archive: archive.into_inner().map_err(archive_error)?,
        reference,
        manifest_digest,
    })
}

/// The layer of a source image: the directories of [`KIT_DIR`], then
/// `kit.yaml` holding `spec`.
fn layer(spec: &str) -> Result<Vec<u8>> {
    let mut layer = tar::Builder::new(Vec::new());
    let mut dir = String::new();
    for part in KIT_DIR.split('/') {
        if !dir.is_empty() {
            dir.push('/');
        }
        dir.push_str(part);
        append(&mut layer, &dir, tar::EntryType::Directory, 0o755, &[])?;
    }
    file(&mut layer, &format!("{dir}/kit.yaml"), spec.as_bytes())?;
    layer.into_inner().map_err(archive_error)
}

/// The annotations of the manifest: the OCI title, description and version,
/// and the descriptor `encoded` as JSON with its capability types.
fn annotations(descriptor: &Value, encoded: &str) -> Value {
    let types: Vec<&str> = descriptor
        .get("capabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("type").and_then(Value::as_str))
        .collect();
    let text = |key: &str| descriptor.get(key).cloned().unwrap_or(Value::Null);
    let built_by = json!({ "name": "docker-sandbox", "version": env!("CARGO_PKG_VERSION") });
    json!({
        "org.opencontainers.image.description": text("description"),
        "org.opencontainers.image.title": text("displayName"),
        "org.opencontainers.image.version": text("version"),
        "vnd.docker.sandbox.kit.built-by": built_by.to_string(),
        "vnd.docker.sandbox.kit.capabilities": types.join(","),
        "vnd.docker.sandbox.kit.descriptor": encoded,
        "vnd.docker.sandbox.kit.schema-version": "3",
    })
}

/// Appends a regular file owned by root and dated 1970 to `builder`.
fn file(builder: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]) -> Result<()> {
    append(builder, path, tar::EntryType::Regular, 0o644, data)
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
        .map_err(archive_error)
}

/// The OCI name of the architecture `arch` (`std::env::consts::ARCH`).
fn architecture(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// `sha256:<hex>` of `bytes`.
pub(super) fn digest(bytes: &[u8]) -> String {
    let mut hex = String::from("sha256:");
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// An [`Error::Archive`] of this module.
fn archive_error(error: impl std::fmt::Display) -> Error {
    Error::Archive {
        operation: OPERATION,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read;

    use super::*;

    /// The files of the archive `bytes`, by path.
    fn files(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut archive = tar::Archive::new(bytes);
        let mut files = BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().display().to_string();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            files.insert(path, content);
        }
        files
    }

    #[test]
    fn digests_are_sha256_in_hexadecimal() {
        assert_eq!(
            digest(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn the_image_is_a_consistent_oci_layout_holding_the_spec() {
        let descriptor = json!({ "schemaVersion": "3", "kind": "mixin", "displayName": "Test",
            "capabilities": [{ "type": "com.docker.sandbox/lifecycle@1" }] });
        let image = source_image("# syntax\n{}\n", &descriptor).unwrap();
        let files = files(&image.archive);
        let index: Value = serde_json::from_slice(&files["index.json"]).unwrap();
        assert_eq!(index["manifests"][0]["digest"], image.manifest_digest);
        assert_eq!(
            index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
            image.reference
        );
        assert!(image.reference.starts_with("sbx-kit-src:spec-") && image.reference.len() == 29);
        for (path, bytes) in &files {
            if let Some(hex) = path.strip_prefix("blobs/sha256/") {
                assert_eq!(digest(bytes), format!("sha256:{hex}"), "{path}");
            }
        }
        let manifest: Value = serde_json::from_slice(
            &files[&format!(
                "blobs/sha256/{}",
                image.manifest_digest.trim_start_matches("sha256:")
            )],
        )
        .unwrap();
        let annotations = &manifest["annotations"];
        assert_eq!(annotations["org.opencontainers.image.title"], "Test");
        assert_eq!(
            annotations["vnd.docker.sandbox.kit.capabilities"],
            "com.docker.sandbox/lifecycle@1"
        );
        assert_eq!(
            annotations["vnd.docker.sandbox.kit.descriptor"],
            descriptor.to_string()
        );
        let layer = &files[&format!(
            "blobs/sha256/{}",
            manifest["layers"][0]["digest"]
                .as_str()
                .unwrap()
                .trim_start_matches("sha256:")
        )];
        assert_eq!(files_of_layer(layer), "# syntax\n{}\n");
        assert_eq!(
            source_image("# syntax\n{}\n", &descriptor).unwrap().archive,
            image.archive,
            "the same descriptor gives the same archive"
        );
    }

    /// The `kit.yaml` of a layer.
    fn files_of_layer(layer: &[u8]) -> String {
        String::from_utf8(files(layer)["usr/share/sandbox/kit/spec/kit.yaml"].clone()).unwrap()
    }

    #[test]
    fn architectures_take_their_oci_names() {
        assert_eq!(architecture("x86_64"), "amd64");
        assert_eq!(architecture("aarch64"), "arm64");
        assert_eq!(architecture("riscv64"), "riscv64");
    }

    #[test]
    fn archive_errors_name_the_operation() {
        let error = archive_error("disk full");
        assert!(
            matches!(error, Error::Archive { operation: OPERATION, ref message } if message == "disk full")
        );
    }
}
