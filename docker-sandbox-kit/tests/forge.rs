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

mod images;

use std::path::Path;

use docker_sandbox::kit::KitPackage;
use docker_sandbox_kit::{
    Agent, Error, KitCache, KitDescriptor, KitForge, KitRef, LifecycleStep, Mixin,
};
use images::{captured_shell, compiled_package, image_export, serving, shell_export};
use serde_json::{Value, json};
use support::{FakeDaemon, FakeDaemonBuilder, Reply, connect};

/// Writes a compiled package of the shell kit and a mixin to `dir`.
fn base_package(dir: &Path) -> KitPackage {
    compiled_package(dir, "shell", "v0.54.1")
}

/// A daemon setting answer of `key` with `value`.
fn setting(key: &str, value: impl Into<Value>) -> Reply {
    let value = value.into();
    Reply::json(
        200,
        &json!({ "key": key, "value": value, "default": value, "source": "default", "type": "bool" }),
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
async fn packages_are_built_in_memory_and_create_sandboxes_from_there() {
    let dir = tempfile::tempdir().unwrap();
    let base = base_package(&dir.path().join("shell"));
    let daemon = permissive().start();
    let client = connect(&daemon).await;

    let package = KitForge::new(client.clone())
        .with_base_package(base)
        .with_mixin(tools())
        .build()
        .await
        .unwrap();
    assert_eq!(package.dir(), None, "nothing was written");
    assert_eq!(package.images().len(), 1);
    // Forged by the version of the crate that goes with docker-sandbox, for
    // the API of this daemon.
    assert!(package.suits(client.health()));

    package.create(&client, "job-42").await.unwrap();
    assert_eq!(
        daemon.request("POST", "/sandbox").json()["kits"][1],
        "/kits/acme-tools"
    );

    // Saved afterwards, it is the package forge writes, and the archive the
    // daemon received from memory is the one written.
    let saved = package.save(dir.path().join("saved")).unwrap();
    let output = saved.dir().unwrap();
    assert_eq!(
        read(output, "kit.json")["forged_by"]["name"],
        "docker-sandbox-kit"
    );
    assert_eq!(
        daemon.request("POST", "/docker/images/load").body,
        std::fs::read(output.join("images/00.oci.tar")).unwrap()
    );
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
async fn compositions_are_checked_before_anything_is_pulled() {
    let dir = tempfile::tempdir().unwrap();
    let pulled = |daemon: &FakeDaemon| {
        daemon
            .requests()
            .iter()
            .any(|r| r.path().starts_with("/docker/images"))
    };
    let daemon = serving(permissive(), shell_export()).start();
    let client = connect(&daemon).await;
    let agent = KitForge::agent(client.clone(), Agent::Shell);
    agent.clone().with_mixin(tools()).check().await.unwrap();
    KitForge::new(client.clone())
        .with_base_package(base_package(&dir.path().join("shell")))
        .with_mixin(tools())
        .check()
        .await
        .unwrap();
    // The arguments are named, but only forging reads what the kit declares.
    agent
        .clone()
        .with_arg("shell.model", "m")
        .check()
        .await
        .unwrap();
    let error = agent.with_arg("acme.model", "m").check().await.unwrap_err();
    assert!(matches!(error, Error::InvalidKit { .. }), "{error}");
    let error = KitForge::new(client).check().await.unwrap_err();
    assert!(
        matches!(error, Error::Forge(ref m) if m.contains("no base kit")),
        "{error}"
    );
    assert!(!pulled(&daemon), "nothing is pulled");

    let signed = serving(permissive(), shell_export())
        .route(
            "GET",
            "/daemon/settings/kit.requireSignature",
            setting("kit.requireSignature", true),
        )
        .start();
    let client = connect(&signed).await;
    let error = KitForge::agent(client.clone(), Agent::Shell)
        .check()
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Forge(ref m) if m.contains("kit.requireSignature is on")),
        "{error}"
    );
    assert!(!pulled(&signed), "nothing is pulled");
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

/// A daemon serving the shell kit whose shortcut for `shell` is `kit`.
fn shortcut(kit: &str) -> FakeDaemonBuilder {
    serving(permissive(), shell_export()).route(
        "GET",
        "/daemon/settings/kit.shortcuts.shell",
        setting("kit.shortcuts.shell", kit),
    )
}

#[tokio::test]
async fn an_agent_resolves_to_the_kit_its_shortcut_names() {
    let (captured, pinned) = captured_shell();
    let daemon = shortcut(&format!("oci://{pinned}")).start();
    let client = connect(&daemon).await;

    let package = KitForge::agent(client.clone(), Agent::Shell)
        .build()
        .await
        .unwrap();

    // As the CLI v0.54.1 records a pinned base: its source as given, its
    // selection by digest; the rest as for the published kit.
    let mut expected = captured.clone();
    expected["storageSource"] = json!(format!("oci://{pinned}"));
    let selection = captured["v3Selection"]
        .to_string()
        .replace("docker.io/sbx/shell:latest", &pinned);
    expected["v3Selection"] = serde_json::from_str(&selection).unwrap();
    assert_eq!(package.kit_artifacts(), [expected]);
    assert_eq!(package.kit_refs(), std::slice::from_ref(&pinned));
    let export = daemon.request("GET", "/docker/images/export");
    assert_eq!(export.query()["name"], pinned);
}

#[tokio::test]
async fn base_kits_are_taken_as_the_cli_names_them() {
    let (captured, pinned) = captured_shell();
    let daemon = serving(permissive(), shell_export()).start();
    let client = connect(&daemon).await;
    for text in ["shell", "docker.io/sbx/shell:latest"] {
        let package = KitForge::new(client.clone())
            .with_kit(KitRef::base(text).unwrap())
            .build()
            .await
            .unwrap();
        assert_eq!(
            package.kit_artifacts(),
            std::slice::from_ref(&captured),
            "{text}"
        );
        assert_eq!(package.kit_refs(), std::slice::from_ref(&pinned));
    }
    for text in ["./kits/shell", "git+https://host/org/kits.git#dir=shell"] {
        let error = KitForge::new(client.clone())
            .with_kit(KitRef::base(text).unwrap())
            .build()
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Forge(ref m) if m.starts_with(&format!("{text} is a kit the sbx CLI builds"))),
            "{error}"
        );
    }
}

/// How many times `daemon` exported an image, once per forging.
fn exports(daemon: &FakeDaemon) -> usize {
    daemon
        .requests()
        .iter()
        .filter(|r| r.path() == "/docker/images/export")
        .count()
}

#[tokio::test]
async fn packages_are_forged_once_per_composition_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = serving(permissive(), shell_export()).start();
    let client = connect(&daemon).await;
    let forge = KitForge::agent(client.clone(), Agent::Shell);

    // In memory: forged once, then taken from the cache, by its clones too.
    let cache = KitCache::new();
    assert_eq!(cache.dir(), None);
    let first = cache.package(&forge).await.unwrap();
    let again = cache.clone().package(&forge).await.unwrap();
    assert_eq!(first.kit_artifacts(), again.kit_artifacts());
    assert_eq!(exports(&daemon), 1);
    // Another composition is forged, and a refresh forges again.
    cache
        .package(&forge.clone().with_mixin(tools()))
        .await
        .unwrap();
    assert_eq!(exports(&daemon), 2);
    cache.refresh(&forge).await.unwrap();
    assert_eq!(exports(&daemon), 3);

    // In a directory: the next cache takes the package from there.
    let saving = KitCache::new().with_dir(dir.path());
    assert_eq!(saving.dir(), Some(dir.path()));
    let saved = saving.package(&forge).await.unwrap();
    let kept = saved.dir().unwrap().to_owned();
    assert!(kept.starts_with(dir.path()));
    assert_eq!(exports(&daemon), 4);
    let next = KitCache::new().with_dir(dir.path());
    let reused = next.package(&forge).await.unwrap();
    assert_eq!(reused.dir(), Some(kept.as_path()));
    assert_eq!(exports(&daemon), 4, "taken from the directory");

    // A package that no longer suits the daemon, forged by another version
    // of the crate, is forged again.
    let metadata = kept.join("kit.json");
    let mut kit: Value =
        serde_json::from_str(&std::fs::read_to_string(&metadata).unwrap()).unwrap();
    kit["forged_by"]["version"] = json!("0.0.1");
    std::fs::write(&metadata, kit.to_string()).unwrap();
    let stale = KitCache::new().with_dir(dir.path());
    stale.package(&forge).await.unwrap();
    assert_eq!(exports(&daemon), 5);
    assert_eq!(
        read(&kept, "kit.json")["forged_by"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    // A failed forging keeps nothing.
    let failing = KitForge::new(client.clone()).with_kit(KitRef::base("./kit").unwrap());
    assert!(cache.package(&failing).await.is_err());
    // A directory a package cannot be saved to.
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    let error = KitCache::new()
        .with_dir(&file)
        .package(&forge)
        .await
        .unwrap_err();
    assert!(
        error.to_string().starts_with("invalid kit package at "),
        "{error}"
    );
}

#[tokio::test]
async fn shortcuts_the_forge_cannot_follow_are_refused_and_empty_ones_ignored() {
    for kit in ["git+https://example.com/kits.git#shell", "/opt/kits/shell"] {
        let daemon = shortcut(kit).start();
        let client = connect(&daemon).await;
        let error = KitForge::agent(client, Agent::Shell)
            .build()
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("(kit.shortcuts.shell), a kit only the sbx CLI builds"),
            "{error}"
        );
    }

    // A daemon that cannot say which kit the agent resolves to.
    let daemon = serving(permissive(), shell_export())
        .route(
            "GET",
            "/daemon/settings/kit.shortcuts.shell",
            Reply::json(500, &json!({ "message": "settings unavailable" })),
        )
        .start();
    let client = connect(&daemon).await;
    let error = KitForge::agent(client, Agent::Shell)
        .build()
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("settings unavailable"),
        "{error}"
    );

    // An empty shortcut is the default of the setting: the published kit.
    let (captured, _) = captured_shell();
    let daemon = shortcut("").start();
    let client = connect(&daemon).await;
    let package = KitForge::agent(client, Agent::Shell).build().await.unwrap();
    assert_eq!(package.kit_artifacts(), [captured]);
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
        (
            "effort",
            "high",
            "declares no argument effort: it declares model",
        ),
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

/// A daemon allowing the kits of `ghcr.io/acme/`, which serves the agent kit
/// `ghcr.io/acme/agent:2`.
fn acme_agent() -> FakeDaemonBuilder {
    serving(
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
}

#[tokio::test]
async fn arguments_are_named_by_their_kit_as_the_cli_takes_them() {
    let daemon = acme_agent().start();
    let client = connect(&daemon).await;
    let forge = || {
        KitForge::new(client.clone())
            .with_base("ghcr.io/acme/agent:2")
            .with_mixin(tools())
    };
    let plain = forge().with_arg("model", "small-1").build().await.unwrap();
    let base = &plain.kit_artifacts()[0];

    // The argument named by its kit, as the CLI takes it: the same artifact,
    // and the request keeps the name as given.
    let qualified = forge()
        .with_arg("agent.model", "small-1")
        .build()
        .await
        .unwrap();
    assert_eq!(qualified.kit_artifacts()[0], *base);
    assert_eq!(
        qualified.request("job-42").unwrap().extra["kit_args"],
        json!({ "agent.model": "small-1" })
    );
    let twice = forge()
        .with_arg("agent.model", "small-1")
        .with_arg("model", "small-2")
        .build()
        .await
        .unwrap_err();
    assert!(
        twice
            .to_string()
            .contains("the argument model is given twice, as model and agent.model"),
        "{twice}"
    );

    for (name, expected) in [
        (
            "acme-tools.model",
            "declares no argument model: a mixin declares none",
        ),
        (
            "other.model",
            "no kit named other is composed (kits: acme-tools, agent), for the argument other.model",
        ),
    ] {
        let error = forge().with_arg(name, "small-1").build().await.unwrap_err();
        assert!(
            matches!(error, Error::InvalidKit { ref message, .. } if message.contains(expected)),
            "{error}"
        );
    }
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
