//! Kit images as a fake daemon pulls and exports them: the published shell
//! kit, as the audit captured its artifact, and any kit image made of a
//! descriptor and a configuration.
//!
//! The tests of `docker-sandbox-kit` forge their packages from these
//! exports, and those of `docker-sandbox-blueprint` include the module by
//! path rather than copy it.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]
#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

use std::path::Path;

use serde_json::{Value, json};

use crate::support::{FakeDaemonBuilder, Reply};

/// The kit artifact of `docker.io/sbx/shell:latest` the CLI wrote, captured
/// by the audit, and the reference it pinned.
pub(crate) fn captured_shell() -> (Value, String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../docker-sandbox/tests/fixtures/post_sandbox_200.json");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let request = &fixture["request"];
    (
        request["kit_artifacts"][0].clone(),
        request["kits"][0].as_str().unwrap().to_owned(),
    )
}

/// The export of the image of a kit whose index digest is `digest`, whose
/// descriptor is `descriptor` and whose configuration starts `entrypoint`
/// then `cmd`, as `GET /docker/images/export` streams it: a layer larger than
/// what is kept, then the small files. A `null` descriptor is an image that
/// is not a kit.
pub(crate) fn image_export(
    digest: &str,
    descriptor: &Value,
    entrypoint: &Value,
    cmd: &Value,
) -> Vec<u8> {
    let hex = digest.trim_start_matches("sha256:");
    let annotations = if descriptor.is_null() {
        json!({})
    } else {
        json!({ "vnd.docker.sandbox.kit.descriptor": descriptor.to_string() })
    };
    let index = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [],
        "annotations": annotations,
    });
    let files: [(String, Vec<u8>); 5] = [
        ("blobs/sha256/1a7e9".to_owned(), vec![7; (2 << 20) + 1]),
        (
            format!("blobs/sha256/{hex}"),
            index.to_string().into_bytes(),
        ),
        (
            "blobs/sha256/c0f19".to_owned(),
            json!({ "config": { "Entrypoint": entrypoint, "Cmd": cmd } })
                .to_string()
                .into_bytes(),
        ),
        (
            "index.json".to_owned(),
            json!({ "manifests": [{ "digest": digest }] })
                .to_string()
                .into_bytes(),
        ),
        (
            "manifest.json".to_owned(),
            json!([{ "Config": "blobs/sha256/c0f19" }])
                .to_string()
                .into_bytes(),
        ),
    ];
    let mut archive = tar::Builder::new(Vec::new());
    for (path, contents) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        archive
            .append_data(&mut header, path, contents.as_slice())
            .unwrap();
    }
    archive.into_inner().unwrap()
}

/// `daemon` able to pull an image and export it as `export`.
pub(crate) fn serving(daemon: FakeDaemonBuilder, export: Vec<u8>) -> FakeDaemonBuilder {
    daemon
        .route(
            "POST",
            "/docker/images/create",
            Reply::ndjson(&[json!({ "status": "Status: Image is up to date" })]),
        )
        .route(
            "GET",
            "/docker/images/export",
            Reply::raw(200, "application/x-tar", export),
        )
}

/// The export of the captured shell kit.
pub(crate) fn shell_export() -> Vec<u8> {
    let (artifact, _) = captured_shell();
    let entrypoint = &artifact["v3Entrypoint"];
    let skipped = entrypoint.as_array().unwrap().len() - 1;
    let cmd: Vec<Value> =
        artifact["manifest"]["runOptions"].as_array().unwrap()[skipped..].to_vec();
    image_export(
        artifact["v3Digest"].as_str().unwrap(),
        &artifact["v3"],
        entrypoint,
        &json!(cmd),
    )
}
