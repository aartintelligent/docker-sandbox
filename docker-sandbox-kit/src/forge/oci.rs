//! The source image of a mixin: the OCI image layout the daemon loads
//! (`POST /docker/images/load`) and finds the mixin by.
//!
//! The layout is the one the `sbx` CLI writes for a local kit, measured by the
//! audit: one layer holding `usr/share/sandbox/kit/spec/kit.yaml`, the
//! descriptor as JSON in the annotations of the manifest, the image named
//! `sbx-kit-src:spec-<hash>`, every entry owned by root and dated 1970, so
//! that the same descriptor always gives the same archive. The daemon checks
//! neither the hash of the name nor the form of `kit.yaml`.
//!
//! The archive is written in memory, entry by entry, with the headers of the
//! `tar` crate: its paths are fixed and short, so that writing it cannot fail.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::descriptor::{Capability, KitDescriptor};
use super::{Producer, to_json};

/// Directory of the descriptor inside the layer.
const KIT_DIR: &str = "usr/share/sandbox/kit/spec";

/// Repository the daemon stores source images under.
const REPOSITORY: &str = "sbx-kit-src";

/// Content of the `oci-layout` file that marks an OCI image layout.
const LAYOUT_MARKER: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;

/// Media type of an image index.
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";

/// Media type of an image manifest.
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// Media type of an image configuration.
const CONFIG_TYPE: &str = "application/vnd.oci.image.config.v1+json";

/// Media type of an uncompressed layer.
const LAYER_TYPE: &str = "application/vnd.oci.image.layer.v1.tar";

/// Annotation of the image holding its kit descriptor, as JSON.
pub(super) const DESCRIPTOR_ANNOTATION: &str = "vnd.docker.sandbox.kit.descriptor";

/// Size of a tar block: headers, and the padding of contents.
const BLOCK: usize = 512;

/// A source image, as an OCI image layout archive.
pub(super) struct SourceImage {
    /// The archive, for `POST /docker/images/load`.
    pub(super) archive: Vec<u8>,
    /// Name the daemon stores it under, e.g. `sbx-kit-src:spec-0123abcd4567`.
    pub(super) reference: String,
    /// Digest of its manifest, `sha256:…`.
    pub(super) manifest_digest: String,
}

/// An OCI image index: the `index.json` of a layout, or the index a
/// published kit resolves to, whose annotations hold its descriptor.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ImageIndex {
    /// Version of the format, `2`.
    #[serde(default)]
    pub(super) schema_version: u32,
    /// Media type of the index.
    #[serde(default)]
    pub(super) media_type: String,
    /// The images it lists.
    #[serde(default)]
    pub(super) manifests: Vec<OciDescriptor>,
    /// Annotations, by key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) annotations: BTreeMap<String, String>,
}

/// A reference to a blob of a layout: its media type, digest and size.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OciDescriptor {
    /// Media type of the blob.
    #[serde(default)]
    pub(super) media_type: String,
    /// Digest of the blob, `sha256:…`.
    pub(super) digest: String,
    /// Size of the blob, in bytes.
    #[serde(default)]
    pub(super) size: u64,
    /// Annotations, by key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) annotations: BTreeMap<String, String>,
}

/// An OCI image manifest.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImageManifest {
    /// Version of the format, `2`.
    schema_version: u32,
    /// Media type of the manifest.
    media_type: &'static str,
    /// The configuration of the image.
    config: OciDescriptor,
    /// The layers of the image.
    layers: Vec<OciDescriptor>,
    /// Annotations, by key.
    annotations: BTreeMap<String, String>,
}

/// An OCI image configuration.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct ImageConfig {
    /// Architecture the image runs on, e.g. `amd64`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    architecture: String,
    /// System the image runs on, `linux`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    os: String,
    /// How a container of the image starts.
    #[serde(default)]
    pub(super) config: ContainerConfig,
    /// The layers, by the digest of their uncompressed content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rootfs: Option<RootFs>,
}

/// How a container of an image starts.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct ContainerConfig {
    /// Program and arguments every command is given to.
    #[serde(
        rename = "Entrypoint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) entrypoint: Option<Vec<String>>,
    /// Default command.
    #[serde(rename = "Cmd", default, skip_serializing_if = "Option::is_none")]
    pub(super) cmd: Option<Vec<String>>,
}

/// The layers of an image, by the digest of their uncompressed content.
#[derive(Debug, Serialize, Deserialize)]
struct RootFs {
    /// Always `layers`.
    #[serde(rename = "type")]
    kind: String,
    /// Digests of the layers.
    diff_ids: Vec<String>,
}

/// The source image of the mixin `descriptor`, whose `kit.yaml` holds `spec`.
pub(super) fn source_image(spec: &str, descriptor: &KitDescriptor) -> SourceImage {
    let layer = layer(spec);
    let layer_digest = digest(&layer);
    let config = json_bytes(&ImageConfig {
        architecture: architecture(std::env::consts::ARCH).to_owned(),
        os: "linux".to_owned(),
        config: ContainerConfig::default(),
        rootfs: Some(RootFs {
            kind: "layers".to_owned(),
            diff_ids: vec![layer_digest.clone()],
        }),
    });
    let config_digest = digest(&config);

    let encoded = descriptor.to_json().to_string();
    let manifest = json_bytes(&ImageManifest {
        schema_version: 2,
        media_type: MANIFEST_TYPE,
        config: blob(CONFIG_TYPE, &config_digest, &config),
        layers: vec![blob(LAYER_TYPE, &layer_digest, &layer)],
        annotations: annotations(descriptor, &encoded),
    });
    let manifest_digest = digest(&manifest);

    let hash = digest(encoded.as_bytes());
    let short = hash.get(7..19).unwrap_or_default();
    let reference = format!("{REPOSITORY}:spec-{short}");
    let mut listed = blob(MANIFEST_TYPE, &manifest_digest, &manifest);
    listed.annotations = BTreeMap::from([
        (
            "io.containerd.image.name".to_owned(),
            format!("docker.io/library/{reference}"),
        ),
        (
            "org.opencontainers.image.ref.name".to_owned(),
            reference.clone(),
        ),
    ]);
    let index = json_bytes(&ImageIndex {
        schema_version: 2,
        media_type: INDEX_TYPE.to_owned(),
        manifests: vec![listed],
        annotations: BTreeMap::new(),
    });

    // The blobs in digest order, then the index and the layout marker, as
    // the CLI writes them.
    let mut blobs = [
        (layer_digest, layer),
        (config_digest, config),
        (manifest_digest.clone(), manifest),
    ];
    blobs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut archive = Vec::new();
    for (blob_digest, bytes) in &blobs {
        let path = format!("blobs/sha256/{}", blob_digest.trim_start_matches("sha256:"));
        file(&mut archive, &path, bytes);
    }
    file(&mut archive, "index.json", &index);
    file(&mut archive, "oci-layout", LAYOUT_MARKER);
    end(&mut archive);
    SourceImage {
        archive,
        reference,
        manifest_digest,
    }
}

/// The reference to the blob `bytes` of `media_type` and digest `digest`.
fn blob(media_type: &str, digest: &str, bytes: &[u8]) -> OciDescriptor {
    OciDescriptor {
        media_type: media_type.to_owned(),
        digest: digest.to_owned(),
        size: bytes.len() as u64,
        annotations: BTreeMap::new(),
    }
}

/// `value` as compact JSON bytes.
fn json_bytes(value: &impl Serialize) -> Vec<u8> {
    to_json(value).to_string().into_bytes()
}

/// The layer of a source image: the directories of [`KIT_DIR`], then
/// `kit.yaml` holding `spec`.
fn layer(spec: &str) -> Vec<u8> {
    let mut layer = Vec::new();
    let mut dir = String::new();
    for part in KIT_DIR.split('/') {
        if !dir.is_empty() {
            dir.push('/');
        }
        dir.push_str(part);
        append(&mut layer, &dir, tar::EntryType::Directory, 0o755, &[]);
    }
    file(&mut layer, &format!("{dir}/kit.yaml"), spec.as_bytes());
    end(&mut layer);
    layer
}

/// The annotations of the manifest: the OCI title, description and version
/// the descriptor gives, and the descriptor `encoded` as JSON with its
/// capability types.
fn annotations(descriptor: &KitDescriptor, encoded: &str) -> BTreeMap<String, String> {
    let spec = descriptor.spec();
    let types: Vec<&str> = spec.capabilities.iter().map(Capability::kind).collect();
    let built_by = to_json(&Producer::this_crate()).to_string();
    let mut annotations = BTreeMap::from([
        ("vnd.docker.sandbox.kit.built-by", built_by),
        ("vnd.docker.sandbox.kit.capabilities", types.join(",")),
        (DESCRIPTOR_ANNOTATION, encoded.to_owned()),
        ("vnd.docker.sandbox.kit.schema-version", "3".to_owned()),
    ]);
    let described = [
        ("org.opencontainers.image.title", &spec.display_name),
        ("org.opencontainers.image.description", &spec.description),
        ("org.opencontainers.image.version", &spec.version),
    ];
    for (key, value) in described {
        if let Some(value) = value {
            annotations.insert(key, value.clone());
        }
    }
    annotations
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// Appends a regular file owned by root and dated 1970 to `archive`.
fn file(archive: &mut Vec<u8>, path: &str, data: &[u8]) {
    append(archive, path, tar::EntryType::Regular, 0o644, data);
}

/// Appends to `archive` one entry owned by root and dated 1970, then its
/// content padded to a whole block, as `tar::Builder` writes them.
fn append(archive: &mut Vec<u8>, path: &str, kind: tar::EntryType, mode: u32, data: &[u8]) {
    let mut header = tar::Header::new_ustar();
    // The paths of a source image are fixed and shorter than the name field:
    // they are written as they are, with nothing to check.
    for (slot, byte) in header.as_old_mut().name.iter_mut().zip(path.bytes()) {
        *slot = byte;
    }
    header.set_entry_type(kind);
    header.set_mode(mode);
    header.set_size(data.len() as u64);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    archive.extend_from_slice(header.as_bytes());
    archive.extend_from_slice(data);
    archive.resize(archive.len().next_multiple_of(BLOCK), 0);
}

/// Ends `archive` with the two empty blocks of the tar format.
fn end(archive: &mut Vec<u8>) {
    archive.resize(archive.len() + 2 * BLOCK, 0);
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

#[cfg(test)]
mod tests {
    use std::io::Read;

    use serde_json::{Value, json};

    use super::super::descriptor::LifecycleStep;
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
        let descriptor = KitDescriptor::mixin("Test").with_install(LifecycleStep::new("true"));
        let image = source_image("# syntax\n{}\n", &descriptor);
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
            descriptor.to_json().to_string()
        );
        assert!(
            annotations
                .get("org.opencontainers.image.description")
                .is_none()
        );
        let built_by: Value = serde_json::from_str(
            annotations["vnd.docker.sandbox.kit.built-by"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(built_by["name"], "docker-sandbox-kit");
        let config_path = format!(
            "blobs/sha256/{}",
            manifest["config"]["digest"]
                .as_str()
                .unwrap()
                .trim_start_matches("sha256:")
        );
        let config: Value = serde_json::from_slice(&files[&config_path]).unwrap();
        assert_eq!(config["os"], "linux");
        assert_eq!(config["config"], json!({}));
        assert_eq!(
            config["rootfs"]["diff_ids"][0],
            manifest["layers"][0]["digest"]
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
            source_image("# syntax\n{}\n", &descriptor).archive,
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
    fn archives_are_written_as_the_tar_crate_writes_them() {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, kind, mode, data) in [
            ("usr", tar::EntryType::Directory, 0o755, &b""[..]),
            ("usr/kit.yaml", tar::EntryType::Regular, 0o644, &b"{}"[..]),
            (
                "blobs/sha256/00",
                tar::EntryType::Regular,
                0o644,
                &[7_u8; 1024][..],
            ),
        ] {
            let mut header = tar::Header::new_ustar();
            header.set_entry_type(kind);
            header.set_mode(mode);
            header.set_size(data.len() as u64);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            builder.append_data(&mut header, path, data).unwrap();
        }
        let mut written = Vec::new();
        append(&mut written, "usr", tar::EntryType::Directory, 0o755, b"");
        file(&mut written, "usr/kit.yaml", b"{}");
        file(&mut written, "blobs/sha256/00", &[7; 1024]);
        end(&mut written);
        assert_eq!(written, builder.into_inner().unwrap());
    }
}
