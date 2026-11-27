//! Kit packages against a fake daemon: image loading, creation and container
//! swap.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

mod support;

use std::path::Path;

use docker_sandbox::kit::{KitPackage, PackageImage};
use docker_sandbox::sandbox::SwapRequest;
use serde_json::{Value, json};
use support::{FakeDaemon, Reply, connect};

/// Writes a package with one image to `dir`.
fn write_package(dir: &Path, template: &Value) {
    std::fs::create_dir_all(dir.join("images")).unwrap();
    std::fs::write(dir.join("images/00.oci.tar"), b"oci-archive").unwrap();
    std::fs::write(dir.join("create-body.json"), template.to_string()).unwrap();
    std::fs::write(
        dir.join("kit.json"),
        json!({ "compiled_with": { "sbx": "v0.54.0" }, "images": [{ "file": "images/00.oci.tar", "ref": "sbx-kit-src:spec-abc" }] }).to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn packages_load_missing_images_and_swap_containers() {
    let dir = tempfile::tempdir().unwrap();
    write_package(
        dir.path(),
        &json!({ "name": "{{name}}", "agent": "shell", "kits": ["docker.io/sbx/shell@sha256:f3", "/kits/mixin"], "kit_artifacts": [{ "k": 0 }, { "k": 1 }] }),
    );
    let daemon = FakeDaemon::builder()
        .route("GET", "/docker/images", Reply::json(200, &json!([])))
        .route(
            "POST",
            "/docker/images/load",
            Reply::json(200, &json!({ "message": "image loaded" })),
        )
        .route(
            "POST",
            "/sandbox/job-42/swap-container",
            Reply::json(201, &json!({ "committed_image_id": "sha256:c", "container_id": "d", "docker_name": "job-42-swap-1" })),
        )
        .start();
    let client = connect(&daemon).await;
    let package = KitPackage::open(dir.path()).unwrap();

    let outcome = package.add_to(&client.sandbox("job-42")).await.unwrap();
    assert_eq!(outcome.docker_name.as_deref(), Some("job-42-swap-1"));
    assert_eq!(
        daemon.request("POST", "/docker/images/load").body,
        b"oci-archive"
    );

    let swap: SwapRequest = serde_json::from_value(
        daemon
            .request("POST", "/sandbox/job-42/swap-container")
            .json(),
    )
    .unwrap();
    assert!(swap.docker_name_override.starts_with("job-42-swap-"));
    assert_eq!(
        swap.kit_refs,
        ["docker.io/sbx/shell@sha256:f3", "/kits/mixin"]
    );
    assert_eq!(swap.kit_artifacts, [json!({ "k": 0 }), json!({ "k": 1 })]);
}

#[tokio::test]
async fn packages_in_memory_load_their_archives_from_memory() {
    let daemon = FakeDaemon::builder()
        .route("GET", "/docker/images", Reply::json(200, &json!([])))
        .route(
            "POST",
            "/docker/images/load",
            Reply::json(200, &json!({ "message": "image loaded" })),
        )
        .start();
    let client = connect(&daemon).await;
    let template = json!({ "name": "{{name}}", "agent": "shell", "kits": ["/kits/mixin"], "kit_artifacts": [{ "k": 0 }] });
    let image = PackageImage::in_memory("sbx-kit-src:spec-abc", b"archive in memory".to_vec());
    let package = KitPackage::in_memory(template, Value::Null, [image]).unwrap();

    assert_eq!(package.load_images(&client).await.unwrap(), 1);
    assert_eq!(
        daemon.request("POST", "/docker/images/load").body,
        b"archive in memory"
    );
}

#[tokio::test]
async fn images_already_held_are_not_reloaded() {
    let dir = tempfile::tempdir().unwrap();
    write_package(dir.path(), &json!({ "name": "{{name}}", "agent": "shell" }));
    let held = json!([{ "repository": "docker.io/library/sbx-kit-src", "tag": "spec-abc", "id": "sha256:1" }]);
    let daemon = FakeDaemon::builder()
        .route("GET", "/docker/images", Reply::json(200, &held))
        .route(
            "POST",
            "/sandbox",
            Reply::ndjson(&[json!({ "result": { "id": "1", "name": "job-42", "agent": "shell", "status": "running" } })]),
        )
        .start();
    let client = connect(&daemon).await;
    let package = KitPackage::open(dir.path()).unwrap();

    assert_eq!(package.load_images(&client).await.unwrap(), 0);
    let created = package.create(&client, "job-42").await.unwrap();
    assert_eq!(created.name, "job-42");
    assert!(
        daemon
            .requests()
            .iter()
            .all(|r| r.path() != "/docker/images/load")
    );
    assert_eq!(daemon.request("POST", "/sandbox").json()["name"], "job-42");
}

#[tokio::test]
async fn mixins_are_installed_in_place_one_artifact_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    write_package(
        dir.path(),
        &json!({ "name": "{{name}}", "agent": "shell", "kits": ["base", "/kits/a", "/kits/b"], "kit_artifacts": [{ "k": 0 }, { "k": 1 }, { "k": 2 }] }),
    );
    let daemon = FakeDaemon::builder()
        .route("GET", "/docker/images", Reply::json(200, &json!([])))
        .route(
            "POST",
            "/docker/images/load",
            Reply::json(200, &json!({ "message": "image loaded" })),
        )
        .route(
            "POST",
            "/sandbox/job-42/kits",
            Reply::json(200, &json!({ "message": "kit added" })),
        )
        .start();
    let client = connect(&daemon).await;
    let package = KitPackage::open(dir.path()).unwrap();

    let installed = package
        .install_into(&client.sandbox("job-42"))
        .await
        .unwrap();
    assert_eq!(installed, 2, "the base kit is skipped");
    let bodies: Vec<Value> = daemon
        .requests()
        .iter()
        .filter(|r| r.path() == "/sandbox/job-42/kits")
        .map(support::Recorded::json)
        .collect();
    assert_eq!(
        bodies,
        [
            json!({ "kit_artifact": { "k": 1 } }),
            json!({ "kit_artifact": { "k": 2 } })
        ]
    );
}

#[tokio::test]
async fn packages_compiled_by_another_sbx_version_still_load() {
    let dir = tempfile::tempdir().unwrap();
    write_package(dir.path(), &json!({ "name": "{{name}}", "agent": "shell" }));
    std::fs::write(
        dir.path().join("kit.json"),
        json!({ "compiled_with": { "sbx": "v0.53.0", "api_version": "0.51.0" }, "images": [{ "file": "images/00.oci.tar", "ref": "sbx-kit-src:spec-abc" }] }).to_string(),
    )
    .unwrap();
    let held = json!([{ "repository": "sbx-kit-src", "tag": "spec-abc", "id": "sha256:1" }]);
    let daemon = FakeDaemon::builder()
        .route("GET", "/docker/images", Reply::json(200, &held))
        .start();
    let client = connect(&daemon).await;
    let package = KitPackage::open(dir.path()).unwrap();

    // The daemon runs v0.54.0, API 0.52.0: warnings are logged, the package
    // is used.
    assert_eq!(package.compiled_with(), Some("v0.53.0"));
    assert_eq!(package.compiled_api(), Some("0.51.0"));
    assert_eq!(package.load_images(&client).await.unwrap(), 0);
}

#[tokio::test]
async fn packages_without_metadata_load_every_archive() {
    let dir = tempfile::tempdir().unwrap();
    write_package(dir.path(), &json!({ "name": "{{name}}", "agent": "shell" }));
    std::fs::remove_file(dir.path().join("kit.json")).unwrap();
    let daemon = FakeDaemon::builder()
        .route("GET", "/docker/images", Reply::json(200, &json!([])))
        .route(
            "POST",
            "/docker/images/load",
            Reply::json(200, &json!({ "message": "image loaded" })),
        )
        .start();
    let client = connect(&daemon).await;
    let package = KitPackage::open(dir.path()).unwrap();

    assert_eq!(package.compiled_with(), None);
    assert_eq!(package.load_images(&client).await.unwrap(), 1);
}
