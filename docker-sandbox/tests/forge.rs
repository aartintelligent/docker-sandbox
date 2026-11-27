//! Kit packages forged without the `sbx` CLI, against a fake daemon: the
//! package written, the sandbox created from it through the socket, and the
//! refusals before anything is written.

#![cfg(feature = "kit-forge")]
#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

mod support;

use std::path::Path;

use docker_sandbox::Error;
use docker_sandbox::kit::{KitDescriptor, KitForge, KitPackage, LifecycleStep, Mixin};
use serde_json::{Value, json};
use support::{FakeDaemon, FakeDaemonBuilder, Reply, connect};

/// Writes a compiled package of the shell kit and a mixin to `dir`.
fn base_package(dir: &Path) -> KitPackage {
    std::fs::create_dir_all(dir.join("images")).unwrap();
    std::fs::write(dir.join("images/00.oci.tar"), b"mixin source").unwrap();
    let template = json!({
        "name": "{{name}}", "agent": "shell", "pull_policy": "missing", "enable_virtiofs_cache": true,
        "kits": ["docker.io/sbx/shell@sha256:f3", "/kits/compiled"],
        "kit_artifacts": [{ "manifest": { "name": "shell", "kind": "sandbox" } }, { "manifest": { "name": "compiled" } }]
    });
    std::fs::write(dir.join("create-body.json"), template.to_string()).unwrap();
    std::fs::write(
        dir.join("kit.json"),
        json!({ "compiled_with": { "sbx": "v0.54.1", "api_version": "0.52.0" }, "images": [{ "file": "images/00.oci.tar", "ref": "sbx-kit-src:spec-old" }] }).to_string(),
    )
    .unwrap();
    KitPackage::open(dir).unwrap()
}

/// A daemon setting answer of `key` with `value`.
fn setting(key: &str, value: bool) -> Reply {
    Reply::json(
        200,
        &json!({ "key": key, "value": value, "default": value, "source": "default", "type": "bool" }),
    )
}

/// A daemon whose kit policy allows local and unsigned kits, which holds no
/// image and creates `job-42`.
fn permissive() -> FakeDaemonBuilder {
    FakeDaemon::builder()
        .route(
            "GET",
            "/daemon/settings/kit.allowLocalKits",
            setting("kit.allowLocalKits", true),
        )
        .route(
            "GET",
            "/daemon/settings/kit.requireSignature",
            setting("kit.requireSignature", false),
        )
        .route("GET", "/docker/images", Reply::json(200, &json!([])))
        .route(
            "POST",
            "/docker/images/load",
            Reply::json(200, &json!({ "message": "image loaded" })),
        )
        .route(
            "POST",
            "/sandbox",
            Reply::ndjson(&[json!({ "result": { "id": "1", "name": "job-42", "agent": "shell", "status": "running" } })]),
        )
}

/// The mixin of the tests: network rules and an install command.
fn tools() -> Mixin {
    let descriptor = KitDescriptor::mixin("Acme tools")
        .with_version("1.0.0")
        .with_network_allow(["api.acme.example:443"])
        .with_install(
            LifecycleStep::new("echo installed > /home/agent/acme.txt")
                .as_user("agent")
                .with_description("Writes a marker file"),
        );
    Mixin::new("/kits/acme-tools", descriptor).unwrap()
}

/// The JSON of a file of the package at `dir`.
fn read(dir: &Path, file: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join(file)).unwrap()).unwrap()
}

#[tokio::test]
async fn forged_packages_create_sandboxes_through_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let base = base_package(&dir.path().join("shell"));
    let daemon = permissive().start();
    let client = connect(&daemon).await;
    let output = dir.path().join("forged");

    let package = KitForge::new(client.clone())
        .with_base_package(base)
        .with_mixin(tools())
        .forge(&output)
        .await
        .unwrap();

    // The base kit of the package only, then the forged mixin.
    assert_eq!(
        package.kit_refs(),
        ["docker.io/sbx/shell@sha256:f3", "/kits/acme-tools"]
    );
    assert_eq!(package.compiled_with(), None, "no sbx compiled it");
    assert_eq!(package.compiled_api(), Some("0.52.0"));
    assert_eq!(package.images().len(), 1);
    let artifacts = package.kit_artifacts();
    let mixin = &artifacts[1];
    assert_eq!(mixin["manifest"]["name"], "acme-tools");
    assert_eq!(
        mixin["manifest"]["template"].as_str(),
        package.images()[0].reference.as_deref()
    );
    assert!(
        mixin["commands"]["install"][0]["command"]
            .as_str()
            .unwrap()
            .starts_with("env -i PATH=")
    );
    assert_eq!(
        mixin["caps"]["network"]["allow"],
        json!(["api.acme.example:443"])
    );
    let metadata = read(&output, "kit.json");
    assert_eq!(metadata["forged_by"]["name"], "docker-sandbox");
    assert_eq!(metadata["mixins"], json!(["/kits/acme-tools"]));
    assert_eq!(read(&output, "create-body.json")["name"], "{{name}}");

    let created = package.create(&client, "job-42").await.unwrap();
    assert_eq!(created.name, "job-42");
    let load = daemon.request("POST", "/docker/images/load");
    assert_eq!(
        load.body,
        std::fs::read(output.join("images/00.oci.tar")).unwrap()
    );
    let body = daemon.request("POST", "/sandbox").json();
    assert_eq!(body["name"], "job-42");
    assert_eq!(body["agent"], "shell");
    assert_eq!(body["kits"][1], "/kits/acme-tools");
    assert_eq!(body["kit_artifacts"][0]["manifest"]["name"], "shell");
}

#[tokio::test]
async fn mixins_read_from_a_directory_are_forged_with_their_spec() {
    let dir = tempfile::tempdir().unwrap();
    let base = base_package(&dir.path().join("shell"));
    let kit = dir.path().join("marker");
    std::fs::create_dir(&kit).unwrap();
    let spec = KitDescriptor::mixin("Marker")
        .with_startup(LifecycleStep::new("date > /tmp/started").in_background())
        .to_spec();
    std::fs::write(kit.join("spec.yaml"), spec).unwrap();
    // A daemon that does not know the kit settings keeps their defaults.
    let daemon = FakeDaemon::builder().start();
    let client = connect(&daemon).await;

    let package = KitForge::new(client)
        .with_base_package(base)
        .with_mixin(Mixin::read(&kit).unwrap())
        .forge(dir.path().join("forged"))
        .await
        .unwrap();
    let mixin = &package.kit_artifacts()[1];
    assert_eq!(
        mixin["commands"]["startup"],
        json!([{ "command": ["sh", "-c", "date > /tmp/started"], "background": true }])
    );
    assert_eq!(
        package.kit_refs()[1],
        kit.canonicalize().unwrap().display().to_string()
    );
}

#[tokio::test]
async fn the_kit_policy_of_the_daemon_is_respected() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (false, false, "kit.allowLocalKits is off"),
        (true, true, "kit.requireSignature is on"),
    ];
    for (local, signature, expected) in cases {
        let daemon = FakeDaemon::builder()
            .route(
                "GET",
                "/daemon/settings/kit.allowLocalKits",
                setting("kit.allowLocalKits", local),
            )
            .route(
                "GET",
                "/daemon/settings/kit.requireSignature",
                setting("kit.requireSignature", signature),
            )
            .start();
        let client = connect(&daemon).await;
        let output = dir.path().join("refused");
        let error = KitForge::new(client)
            .with_base_package(base_package(&dir.path().join("shell")))
            .with_mixin(tools())
            .forge(&output)
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Forge(ref m) if m.contains(expected)),
            "{error}"
        );
        assert!(!output.exists(), "nothing is written");
    }

    // A failure to read a setting is not taken for a default.
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/daemon/settings/kit.allowLocalKits",
            Reply::json(500, &json!({ "message": "settings unavailable" })),
        )
        .start();
    let error = KitForge::new(connect(&daemon).await)
        .with_base_package(base_package(&dir.path().join("shell")))
        .with_mixin(tools())
        .forge(dir.path().join("refused"))
        .await
        .unwrap_err();
    assert!(error.is_transient(), "{error}");
}

#[tokio::test]
async fn incomplete_compositions_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = permissive().start();
    let client = connect(&daemon).await;
    let base = base_package(&dir.path().join("shell"));
    let output = dir.path().join("refused");

    let refusal = |forge: KitForge| {
        let output = output.clone();
        async move { forge.forge(output).await.unwrap_err() }
    };
    let no_base = refusal(KitForge::new(client.clone()).with_mixin(tools())).await;
    assert!(matches!(no_base, Error::Forge(ref m) if m.contains("no base kit")));
    let no_mixin = refusal(KitForge::new(client.clone()).with_base_package(base.clone())).await;
    assert!(matches!(no_mixin, Error::Forge(ref m) if m.contains("no mixin")));
    let twice = refusal(
        KitForge::new(client.clone())
            .with_base_package(base.clone())
            .with_mixin(tools())
            .with_mixin(tools()),
    )
    .await;
    assert!(matches!(twice, Error::Forge(ref m) if m.contains("acme-tools")));
    let shell = Mixin::new("/kits/shell", KitDescriptor::mixin("Shell")).unwrap();
    let base_name = refusal(
        KitForge::new(client)
            .with_base_package(base)
            .with_mixin(shell),
    )
    .await;
    assert!(matches!(base_name, Error::Forge(ref m) if m.contains("shell")));
    assert!(!output.exists());
}
