//! Kit packages forged without the `sbx` CLI, against a fake daemon: the
//! package written, the base kit rebuilt from the export of its image and
//! compared with the artifact the CLI wrote for it, the sandbox created from
//! the package through the socket, and the refusals before anything is
//! written.

#![cfg(feature = "forge")]
#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

// The fake daemon of the tests of `docker-sandbox`, shared rather than copied.
#[path = "../../docker-sandbox/tests/support/mod.rs"]
mod support;

use std::path::Path;

use docker_sandbox::kit::KitPackage;
use docker_sandbox_kit::{Agent, Error, KitDescriptor, KitForge, LifecycleStep, Mixin};
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
fn setting(key: &str, value: impl Into<Value>) -> Reply {
    let value = value.into();
    Reply::json(
        200,
        &json!({ "key": key, "value": value, "default": value, "source": "default", "type": "bool" }),
    )
}

/// The kit artifact of `docker.io/sbx/shell:latest` the CLI wrote, captured
/// by the audit, and the reference it pinned.
fn captured_shell() -> (Value, String) {
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
fn image_export(digest: &str, descriptor: &Value, entrypoint: &Value, cmd: &Value) -> Vec<u8> {
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
fn serving(daemon: FakeDaemonBuilder, export: Vec<u8>) -> FakeDaemonBuilder {
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
fn shell_export() -> Vec<u8> {
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

/// The descriptor of an agent kit: an argument used by its install command,
/// a network rule and a credential.
fn agent_descriptor() -> Value {
    json!({
        "schemaVersion": "3",
        "kind": "workload",
        "displayName": "Agent",
        "args": { "model": { "default": "base-1", "env": "AGENT_MODEL", "pattern": "[A-Za-z0-9._-]*" } },
        "capabilities": [
            { "type": "com.docker.sandbox/network-policy@1", "config": { "runtime": { "allow": ["api.agent.example:443"] } } },
            { "type": "com.docker.sandbox/credential@1", "config": { "service": "agent", "apiKey": {
                "inject": [{ "domain": "api.agent.example", "header": "x-api-key", "format": "%s" }] } } },
            { "type": "com.docker.sandbox/lifecycle@1", "config": { "install": [
                { "command": "echo ${{ kit.args.model }} > /home/agent/model", "user": "agent" }] } }
        ]
    })
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
    assert_eq!(metadata["forged_by"]["name"], "docker-sandbox-kit");
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
    let with_args = refusal(
        KitForge::new(client.clone())
            .with_base_package(base.clone())
            .with_arg("model", "x")
            .with_mixin(tools()),
    )
    .await;
    assert!(matches!(with_args, Error::Forge(ref m) if m.contains("kit arguments apply")));
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

#[tokio::test]
async fn published_base_kits_are_rebuilt_as_the_cli_writes_them() {
    let dir = tempfile::tempdir().unwrap();
    let (captured, pinned) = captured_shell();
    let daemon = serving(permissive(), shell_export()).start();
    let client = connect(&daemon).await;
    let output = dir.path().join("shell");

    // The shell kit named as an agent: its published image.
    let package = KitForge::agent(client.clone(), Agent::Shell)
        .forge(&output)
        .await
        .unwrap();

    // Field by field, the artifact the CLI wrote for the same image.
    assert_eq!(package.kit_artifacts(), [captured]);
    assert_eq!(package.kit_refs(), std::slice::from_ref(&pinned));
    assert!(
        package.images().is_empty(),
        "the daemon pulls the base image"
    );
    let pull = daemon.request("POST", "/docker/images/create");
    assert_eq!(pull.query()["fromImage"], "docker.io/sbx/shell:latest");
    let export = daemon.request("GET", "/docker/images/export");
    assert_eq!(export.query()["name"], "docker.io/sbx/shell:latest");
    let metadata = read(&output, "kit.json");
    assert_eq!(metadata["base_image"], "docker.io/sbx/shell:latest");
    assert_eq!(metadata["base"], pinned);

    let created = package.create(&client, "job-42").await.unwrap();
    assert_eq!(created.name, "job-42");
    let body = daemon.request("POST", "/sandbox").json();
    assert_eq!(body["agent"], "shell");
    assert_eq!(body["kits"], json!([pinned]));
    assert_eq!(body["pull_policy"], "missing");
    assert_eq!(body["enable_virtiofs_cache"], true);
    assert!(body.get("kit_args").is_none());
    assert!(
        daemon
            .requests()
            .iter()
            .all(|r| r.path() != "/docker/images/load")
    );
}

#[tokio::test]
async fn arguments_and_mixins_compose_on_a_published_base() {
    let dir = tempfile::tempdir().unwrap();
    let export = image_export(
        "sha256:a9e7",
        &agent_descriptor(),
        &json!(["agent"]),
        &json!([]),
    );
    let daemon = serving(permissive(), export).start();
    let client = connect(&daemon).await;
    let forge = KitForge::new(client.clone())
        .with_base("ghcr.io/acme/agent:2")
        .with_mixin(tools());
    // `ghcr.io/` is not a default source of kits.
    let refused = forge.forge(dir.path().join("refused")).await.unwrap_err();
    assert!(
        matches!(refused, Error::Forge(ref m) if m.contains("only allows kits from docker.io/, dhi.io/")),
        "{refused}"
    );

    let daemon = serving(
        permissive().route(
            "GET",
            "/daemon/settings/kit.allowedSources",
            setting("kit.allowedSources", json!(["ghcr.io/acme/"])),
        ),
        image_export(
            "sha256:a9e7",
            &agent_descriptor(),
            &json!(["agent"]),
            &json!([]),
        ),
    )
    .start();
    let client = connect(&daemon).await;
    let package = KitForge::new(client.clone())
        .with_base("ghcr.io/acme/agent:2")
        .with_arg("model", "small-1")
        .with_mixin(tools())
        .forge(dir.path().join("agent"))
        .await
        .unwrap();
    assert_eq!(
        package.kit_refs(),
        ["ghcr.io/acme/agent@sha256:a9e7", "/kits/acme-tools"]
    );
    let artifacts = package.kit_artifacts();
    let base = &artifacts[0];
    assert_eq!(base["environment"]["variables"]["AGENT_MODEL"], "small-1");
    assert_eq!(base["v3ArgValues"], json!({ "model": "small-1" }));
    assert_eq!(base["credentials"][0]["service"], "agent");
    assert!(
        base["commands"]["install"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with("sh -c 'echo small-1 > /home/agent/model'")
    );
    assert_eq!(base["storageSource"], "oci://ghcr.io/acme/agent:2");
    assert!(base.get("kitHubPublication").is_none());
    assert_eq!(artifacts[1]["manifest"]["name"], "acme-tools");
    assert_eq!(package.images().len(), 1, "the source image of the mixin");

    package.create(&client, "job-42").await.unwrap();
    let body = daemon.request("POST", "/sandbox").json();
    assert_eq!(body["agent"], "agent");
    assert_eq!(body["kit_args"], json!({ "model": "small-1" }));

    for (name, value, expected) in [
        (
            "model",
            "it's",
            "does not match the pattern of the argument model",
        ),
        ("effort", "high", "declares no argument effort"),
    ] {
        let error = KitForge::new(client.clone())
            .with_base("ghcr.io/acme/agent:2")
            .with_arg(name, value)
            .forge(dir.path().join("refused"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::InvalidKit { ref message, .. } if message.contains(expected)),
            "{error}"
        );
    }
    assert!(!dir.path().join("refused").exists());
}

#[tokio::test]
async fn the_sources_and_signatures_of_published_kits_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        ("kit.allowedSources", json!(["*"]), None),
        (
            "kit.allowedSources",
            json!("docker.io/"),
            Some("not a list of prefixes"),
        ),
        (
            "kit.allowedSources",
            json!(["dhi.io/"]),
            Some("not docker.io/sbx/shell"),
        ),
        (
            "kit.requireSignature",
            json!(true),
            Some("requires signed kits"),
        ),
    ];
    for (key, value, refusal) in cases {
        let daemon = serving(
            permissive().route(
                "GET",
                &format!("/daemon/settings/{key}"),
                setting(key, value),
            ),
            shell_export(),
        )
        .start();
        let forged = KitForge::new(connect(&daemon).await)
            .with_base("sbx/shell")
            .forge(dir.path().join("shell"))
            .await;
        match refusal {
            None => assert_eq!(forged.unwrap().kit_refs().len(), 1),
            Some(expected) => {
                let error = forged.unwrap_err();
                assert!(
                    matches!(error, Error::Forge(ref m) if m.contains(expected)),
                    "{error}"
                );
            }
        }
    }
}

/// The refusal of `daemon` to forge into `output` a package on the published
/// base kit `reference`.
async fn refused(daemon: &FakeDaemon, reference: &str, output: &Path) -> Error {
    KitForge::new(connect(daemon).await)
        .with_base(reference)
        .forge(output)
        .await
        .unwrap_err()
}

#[tokio::test]
async fn images_that_are_not_the_kit_asked_for_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("refused");
    // A pinned reference exported as another image.
    let daemon = serving(permissive(), shell_export()).start();
    let error = refused(&daemon, "docker.io/sbx/shell:latest@sha256:0bad", &output).await;
    // The digest the export holds, that of the shell kit the audit recorded.
    let exported = format!(
        "was exported as {}",
        captured_shell().0["v3Digest"].as_str().unwrap()
    );
    assert!(
        matches!(error, Error::Forge(ref m) if m.contains(&exported)),
        "{error}"
    );
    let pull = daemon.request("POST", "/docker/images/create");
    assert_eq!(pull.query()["fromImage"], "docker.io/sbx/shell@sha256:0bad");

    // An image without a kit descriptor, a mixin, an export cut short, an
    // image the daemon cannot export, a reference that is not one.
    let plain = image_export("sha256:0c1", &Value::Null, &json!([]), &json!([]));
    let mut cut = shell_export();
    cut.truncate(5000);
    let mixin = image_export(
        "sha256:0c2",
        &json!({ "schemaVersion": "3", "kind": "mixin" }),
        &json!([]),
        &json!([]),
    );
    for (export, expected) in [
        (
            plain,
            "invalid kit docker.io/sbx/shell:latest: the image is not a kit",
        ),
        (mixin, "a base kit is a workload kit"),
        (cut, "the export ended in the middle of an entry"),
    ] {
        let daemon = serving(permissive(), export).start();
        let error = refused(&daemon, "docker.io/sbx/shell", &output).await;
        assert!(error.to_string().contains(expected), "{error}");
    }
    let daemon = permissive()
        .route("POST", "/docker/images/create", Reply::ndjson(&[]))
        .route(
            "GET",
            "/docker/images/export",
            Reply::json(404, &json!({ "message": "image not found" })),
        )
        .start();
    assert!(
        refused(&daemon, "docker.io/sbx/shell", &output)
            .await
            .is_not_found()
    );
    let error = refused(&daemon, "docker.io/sbx/shell@latest", &output).await;
    assert!(matches!(error, Error::InvalidKit { .. }), "{error}");
    assert!(!output.exists());
}

#[tokio::test]
async fn failures_of_the_daemon_and_of_the_disk_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("refused");

    // The image cannot be pulled, or its export breaks off or is malformed.
    let daemon = permissive()
        .route(
            "POST",
            "/docker/images/create",
            Reply::json(500, &json!({ "message": "registry unreachable" })),
        )
        .start();
    let error = refused(&daemon, "docker.io/sbx/shell", &output).await;
    assert!(
        error.to_string().contains("registry unreachable"),
        "{error}"
    );
    let truncated = permissive()
        .route("POST", "/docker/images/create", Reply::ndjson(&[]))
        .route(
            "GET",
            "/docker/images/export",
            Reply::raw(200, "application/x-tar", shell_export()).truncated(),
        )
        .start();
    let error = refused(&truncated, "docker.io/sbx/shell", &output).await;
    assert!(
        matches!(error, Error::Client(docker_sandbox::Error::Http { .. })),
        "{error}"
    );
    let mut malformed = shell_export();
    malformed[124..136].copy_from_slice(b"zzzzzzzzzzz\0");
    let daemon = serving(permissive(), malformed).start();
    let error = refused(&daemon, "docker.io/sbx/shell", &output).await;
    assert!(error.to_string().contains("invalid size field"), "{error}");

    // A setting the daemon cannot read is not taken for its default.
    for key in ["kit.requireSignature", "kit.allowedSources"] {
        let daemon = serving(
            permissive().route(
                "GET",
                &format!("/daemon/settings/{key}"),
                Reply::json(500, &json!({ "message": "settings unavailable" })),
            ),
            shell_export(),
        )
        .start();
        let error = refused(&daemon, "docker.io/sbx/shell", &output).await;
        assert!(error.is_transient(), "{key}: {error}");
    }

    // The package cannot be written over a file.
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    let daemon = serving(permissive(), shell_export()).start();
    let error = refused(&daemon, "docker.io/sbx/shell", &file).await;
    assert!(
        matches!(error, Error::Client(docker_sandbox::Error::Package { .. })),
        "{error}"
    );
    assert!(!output.exists());
}

#[tokio::test]
async fn compiled_packages_without_a_creation_template_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("broken");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("create-body.json"),
        json!({ "name": "{{name}}", "kits": ["docker.io/sbx/shell@sha256:f3"] }).to_string(),
    )
    .unwrap();
    let daemon = permissive().start();
    let error = KitForge::new(connect(&daemon).await)
        .with_base_package(KitPackage::open(&package).unwrap())
        .with_mixin(tools())
        .forge(dir.path().join("refused"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            Error::Client(docker_sandbox::Error::InvalidRequest(_))
        ),
        "{error}"
    );
}
