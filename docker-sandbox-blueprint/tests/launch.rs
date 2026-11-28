//! Sandboxes launched from their spec against a fake daemon: the creation
//! request, the keys pushed, the report, the lease and its shutdown, and the
//! sandbox removed or kept when a launch fails once it exists.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

// The fake daemon of the tests of `docker-sandbox`, shared rather than copied.
#[path = "../../docker-sandbox/tests/support/mod.rs"]
mod support;

// The kit images of the tests of `docker-sandbox-kit`, shared the same way.
#[path = "../../docker-sandbox-kit/tests/images/mod.rs"]
mod images;

use std::time::Duration;

use docker_sandbox::sandbox::Workspace;
use docker_sandbox_blueprint::{Error, KeySpec, Keys, LaunchStep, SandboxSpec};
use docker_sandbox_kit::KitCache;
use docker_sandbox_kit::{Agent, McpServer};
use images::{compiled_package, image_export, serving, shell_export};
use serde_json::{Value, json};
use support::{FakeDaemon, FakeDaemonBuilder, Recorded, Reply, connect, sandbox_json};

/// A daemon setting answer of `key` with `value`.
fn setting(key: &str, value: impl Into<Value>) -> Reply {
    let value = value.into();
    Reply::json(
        200,
        &json!({ "key": key, "value": value, "default": value, "source": "default", "type": "bool" }),
    )
}

/// A daemon that forges a kit from `export`, creates `job-42`, starts it and
/// takes its keys.
fn launching(export: Vec<u8>) -> FakeDaemonBuilder {
    let builder = FakeDaemon::builder()
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
            Reply::ndjson(&[json!({ "result": {
                "id": "11765de3", "name": "job-42", "agent": "claude", "status": "stopped",
                "withheld": ["anthropic"]
            } })]),
        )
        .route("GET", "/sandbox/job-42/session", Reply::empty(200))
        .route(
            "POST",
            "/sandbox/job-42/credentials",
            Reply::json(200, &json!({ "message": "Credentials updated" })),
        )
        .route(
            "POST",
            "/sandbox/job-42/start",
            Reply::json(200, &sandbox_json("job-42", "running")),
        )
        .route("DELETE", "/sandbox/job-42", Reply::empty(204))
        .route(
            "GET",
            "/policy/network/rules",
            Reply::json(
                200,
                &json!({ "rules": [{
                    "id": "default-allow-all", "name": "default-allow-all", "decision": "allow",
                    "actions": ["net:connect:tcp"], "resources": ["**"], "resource_type": "network",
                    "scope": "global"
                }] }),
            ),
        );
    serving(builder, export)
}

/// The requests of `daemon` for `method path`.
fn sent(daemon: &FakeDaemon, method: &str, path: &str) -> Vec<Recorded> {
    daemon
        .requests()
        .into_iter()
        .filter(|r| r.method == method && r.path() == path)
        .collect()
}

/// A claude spec with a field of each kind the launch applies.
fn claude() -> SandboxSpec {
    SandboxSpec::agent(Agent::Claude)
        .with_workspace("/srv/project")
        .with_mount(Workspace::new("/srv/docs").read_only().at("/docs"))
        .with_network_allow(["api.example.com:443"])
        .with_network_deny(["*.pastebin.com"])
        .with_key(KeySpec::for_hosts("ACME_TOKEN", ["api.acme.example"]))
        .with_mcp(McpServer::host("tools", 47813))
        .with_mcp(McpServer::stdio("files", "mcp-fs"))
        .with_env("LOG", "debug")
        .with_cpus(2)
        .with_memory("4g")
}

#[tokio::test]
async fn a_spec_launches_its_sandbox_and_the_lease_removes_it() {
    let daemon = launching(shell_export()).start();
    let client = connect(&daemon).await;
    let keys = Keys::new().with_key("ACME_TOKEN", "a dummy key");
    let lease = claude().launch(&client, "job-42", &keys).await.unwrap();
    assert_eq!(lease.sandbox().name(), "job-42");
    assert_eq!(lease.created().id, "11765de3");
    // The agent the base kit declares, the kit rebuilt from the export.
    assert_eq!(
        lease.agent().map(|agent| agent.name.as_str()),
        Some("claude")
    );

    // The creation request carries the variables, the token, the
    // placeholder, the workspace, the mounts, the resources and the kits.
    let created = sent(&daemon, "POST", "/sandbox")[0].json();
    let environment = &created["environment"];
    assert_eq!(environment["LOG"], "debug");
    let token = lease.mcp_token("tools").unwrap().expose_secret();
    assert_eq!(token.len(), 64);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(environment["DOCKER_SANDBOX_MCP_TOOLS_TOKEN"], token);
    let placeholder = environment["ACME_TOKEN"].as_str().unwrap();
    assert!(placeholder.starts_with("sbx-cs-"), "{placeholder}");
    assert!(!created.to_string().contains("a dummy key"));
    assert_eq!(created["workspace"], "/srv/project");
    assert_eq!(
        created["additional_workspaces"],
        json!([{ "dir": "/srv/docs", "read_only": true, "target": "/docs" }])
    );
    assert_eq!(created["cpus"], 2);
    assert_eq!(created["memory"], "4g");
    assert!(
        lease.mcp_token("files").is_none(),
        "a stdio server takes no token"
    );

    // The key itself goes to the proxy, after the start.
    let updates = sent(&daemon, "POST", "/sandbox/job-42/credentials");
    let pushed = updates.last().unwrap().json().to_string();
    assert!(pushed.contains("a dummy key") && pushed.contains("api.acme.example"));
    assert_eq!(sent(&daemon, "POST", "/sandbox/job-42/start").len(), 1);

    let report = lease.report();
    assert_eq!(
        report.kits[1..],
        [
            "/kits/docker-sandbox-network",
            "/kits/docker-sandbox-mcp-claude"
        ]
    );
    assert_eq!(report.network.allow, ["api.example.com:443"]);
    assert_eq!(report.network.deny, ["*.pastebin.com"]);
    assert!(
        report
            .network
            .warning
            .as_deref()
            .unwrap()
            .contains("default-allow-all")
    );
    assert_eq!(report.keys[0].env, "ACME_TOKEN");
    assert_eq!(report.keys[0].hosts, ["api.acme.example"]);
    assert_eq!(report.mcp[0].name, "tools");
    assert_eq!(report.mcp[0].kind, "host");
    assert_eq!(report.mcp[1].kind, "stdio");
    assert_eq!(report.mcp[0].agent, "claude");
    assert_eq!(report.withheld, ["anthropic"]);
    let written = serde_json::to_string(report).unwrap();
    assert!(!written.contains("a dummy key") && !written.contains(token));

    lease.shutdown().await.unwrap();
    let deleted = sent(&daemon, "DELETE", "/sandbox/job-42");
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].query()["expected_id"], "11765de3");
}

#[tokio::test]
async fn a_launch_failing_once_the_sandbox_exists_removes_it_unless_kept() {
    let failing_start = || {
        launching(shell_export()).route(
            "POST",
            "/sandbox/job-42/start",
            Reply::json(500, &json!({ "message": "the VM did not start" })),
        )
    };
    let spec = SandboxSpec::agent(Agent::Shell);
    let daemon = failing_start().start();
    let error = spec
        .launch(&connect(&daemon).await, "job-42", &Keys::new())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            Error::Launch { ref sandbox, ref id, step: LaunchStep::Start, kept: false, .. }
                if sandbox == "job-42" && id == "11765de3"
        ),
        "{error}"
    );
    assert_eq!(
        error.to_string().split(": ").next().unwrap(),
        "launching the sandbox job-42 failed when starting it, and the sandbox was removed"
    );
    assert_eq!(sent(&daemon, "DELETE", "/sandbox/job-42").len(), 1);

    // Kept by the spec.
    let daemon = failing_start().start();
    let error = spec
        .clone()
        .keep_on_failure()
        .launch(&connect(&daemon).await, "job-42", &Keys::new())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("and the sandbox stays"),
        "{error}"
    );
    assert!(sent(&daemon, "DELETE", "/sandbox/job-42").is_empty());

    // Kept because it could not be removed.
    let daemon = failing_start()
        .route(
            "DELETE",
            "/sandbox/job-42",
            Reply::json(500, &json!({ "message": "busy" })),
        )
        .start();
    let error = spec
        .launch(&connect(&daemon).await, "job-42", &Keys::new())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Launch { kept: true, .. }), "{error}");

    // The keys refused by the proxy: the empty update of the start passes,
    // the keys do not.
    let daemon = launching(shell_export())
        .route_sequence(
            "POST",
            "/sandbox/job-42/credentials",
            vec![
                Reply::json(200, &json!({ "message": "Credentials updated" })),
                Reply::json(500, &json!({ "message": "secret store unavailable" })),
            ],
        )
        .start();
    let error = spec
        .clone()
        .with_key(KeySpec::for_hosts("ACME_TOKEN", ["api.acme.example"]))
        .launch(
            &connect(&daemon).await,
            "job-42",
            &Keys::new().with_key("ACME_TOKEN", "a dummy key"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            Error::Launch {
                step: LaunchStep::Keys,
                kept: false,
                ..
            }
        ),
        "{error}"
    );
    assert!(
        error.to_string().contains("when pushing its keys"),
        "{error}"
    );
}

#[tokio::test]
async fn keys_and_the_hosts_of_their_service_are_checked_before_the_creation() {
    let agent = image_export(
        "sha256:a9e7",
        &json!({
            "schemaVersion": "3",
            "kind": "workload",
            "displayName": "Agent",
            "capabilities": [
                { "type": "com.docker.sandbox/credential@1", "config": { "service": "agent", "apiKey": {
                    "inject": [{ "domain": "api.agent.example", "header": "x-api-key", "format": "%s" }] } } }
            ]
        }),
        &json!(["agent"]),
        &json!([]),
    );
    let daemon = launching(agent).start();
    let client = connect(&daemon).await;
    let spec = SandboxSpec::base("docker.io/acme/agent:2")
        .with_key(KeySpec::for_service("AGENT_KEY", "agent"));
    let keys = Keys::new().with_key("AGENT_KEY", "a dummy key");

    let error = spec
        .launch(&client, "job-42", &Keys::new())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("the key AGENT_KEY is given no value")
    );
    let error = spec
        .clone()
        .with_network_deny(["*.agent.example"])
        .launch(&client, "job-42", &keys)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid sandbox spec: network.deny: *.agent.example refuses api.agent.example, where the \
         key AGENT_KEY goes; a refusal wins over any allowance"
    );
    let error = SandboxSpec::base("docker.io/acme/agent:2")
        .with_key(KeySpec::for_service("OTHER_KEY", "other"))
        .launch(&client, "job-42", &Keys::new().with_key("OTHER_KEY", "x"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Client(_)), "{error}");
    assert!(
        sent(&daemon, "POST", "/sandbox").is_empty(),
        "nothing is created"
    );

    // A key of the service reaches the hosts the kit declares for it.
    let lease = spec
        .clone()
        .with_network_deny(["*.pastebin.com"])
        .launch(&client, "job-42", &keys)
        .await
        .unwrap();
    assert_eq!(lease.report().keys[0].hosts, ["api.agent.example"]);
    assert_eq!(lease.report().network.warning, None);
    assert!(lease.report().mcp.is_empty());
    assert!(
        sent(&daemon, "GET", "/policy/network/rules").is_empty(),
        "no allowance, no question"
    );
    lease.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_lease_is_detached_or_dropped_without_removing_its_sandbox() {
    let daemon = launching(shell_export())
        .route(
            "GET",
            "/policy/network/rules",
            Reply::json(200, &json!({ "rules": [] })),
        )
        .start();
    let client = connect(&daemon).await;
    let spec = SandboxSpec::agent(Agent::Shell)
        .with_network_allow(["api.example.com"])
        .with_cloned_workspace("/srv/repository");
    let lease = spec.launch(&client, "job-42", &Keys::new()).await.unwrap();
    assert_eq!(lease.report().network.warning, None);
    let created = sent(&daemon, "POST", "/sandbox")[0].json();
    assert_eq!(
        (&created["workspace"], &created["clone"]),
        (&json!("/srv/repository"), &json!(true))
    );
    let (sandbox, session) = lease.detach();
    assert_eq!((sandbox.name(), session.sandbox()), ("job-42", "job-42"));
    drop(session);

    let lease = spec.launch(&client, "job-42", &Keys::new()).await.unwrap();
    drop(lease);
    assert!(sent(&daemon, "DELETE", "/sandbox/job-42").is_empty());

    // A shutdown the daemon refuses leaves the sandbox.
    let daemon = launching(shell_export())
        .route(
            "DELETE",
            "/sandbox/job-42",
            Reply::json(500, &json!({ "message": "busy" })),
        )
        .start();
    let lease = SandboxSpec::agent(Agent::Shell)
        .launch(&connect(&daemon).await, "job-42", &Keys::new())
        .await
        .unwrap();
    assert!(matches!(lease.shutdown().await, Err(Error::Client(_))));
}

/// Writes to `dir` a package of the kit `kit` alone, compiled by the `sbx`
/// version of the fake daemon.
fn base_package(dir: &std::path::Path, kit: &str) {
    compiled_package(dir, kit, "v0.54.0");
    let template = json!({
        "name": "{{name}}", "agent": kit,
        "kits": [format!("docker.io/sbx/{kit}@sha256:f3")],
        "kit_artifacts": [{ "manifest": { "name": kit, "kind": "sandbox" } }]
    });
    std::fs::write(dir.join("create-body.json"), template.to_string()).unwrap();
    std::fs::write(
        dir.join("kit.json"),
        json!({ "compiled_with": { "sbx": "v0.54.0", "api_version": "0.52.0" } }).to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn a_cache_forges_the_kit_of_a_spec_once_for_all_its_sandboxes() {
    let daemon = launching(shell_export()).start();
    let client = connect(&daemon).await;
    let cache = KitCache::new();
    let spec = SandboxSpec::agent(Agent::Shell).with_network_deny(["*.pastebin.com"]);
    for _ in 0..2 {
        let lease = spec
            .launch_with(&client, "job-42", &Keys::new(), &cache)
            .await
            .unwrap();
        lease.shutdown().await.unwrap();
    }
    assert_eq!(
        sent(&daemon, "GET", "/docker/images/export").len(),
        1,
        "forged once"
    );
    assert_eq!(sent(&daemon, "POST", "/sandbox").len(), 2);
}

#[tokio::test]
async fn a_compiled_package_is_launched_as_it_is_or_under_the_mixins_of_the_spec() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = launching(shell_export()).start();
    let client = connect(&daemon).await;

    // As it is: no forge, the kits of the package, its images loaded.
    let compiled = compiled_package(&dir.path().join("compiled"), "shell", "v0.54.0");
    let spec = SandboxSpec::package(dir.path().join("compiled"));
    spec.check(&client).await.unwrap();
    let lease = spec.launch(&client, "job-42", &Keys::new()).await.unwrap();
    assert_eq!(lease.report().kits, compiled.kit_refs());
    assert_eq!(
        sent(&daemon, "POST", "/sandbox")[0].json()["kits"],
        json!(compiled.kit_refs())
    );
    assert_eq!(sent(&daemon, "POST", "/docker/images/load").len(), 1);
    lease.shutdown().await.unwrap();

    // Under the mixins of the spec: forged on its base kit, whose image is
    // never exported again.
    base_package(&dir.path().join("claude"), "claude");
    let lease = SandboxSpec::package(dir.path().join("claude"))
        .with_network_deny(["*.pastebin.com"])
        .with_mcp(McpServer::host("tools", 47813))
        .launch(&client, "job-42", &Keys::new())
        .await
        .unwrap();
    assert_eq!(
        lease.report().kits[1..],
        [
            "/kits/docker-sandbox-network",
            "/kits/docker-sandbox-mcp-claude"
        ]
    );
    // The agent of the MCP servers is the one the package declares.
    assert_eq!(lease.report().mcp[0].agent, "claude");
    assert!(lease.mcp_token("tools").is_some());
    assert!(sent(&daemon, "GET", "/docker/images/export").is_empty());
    lease.shutdown().await.unwrap();

    // A package with mixins of its own takes none of the spec.
    let error = SandboxSpec::package(dir.path().join("compiled"))
        .with_network_deny(["*.pastebin.com"])
        .check(&client)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("composes mixins of its own (/kits/compiled)"),
        "{error}"
    );
    // A package whose agent has no MCP adapter takes no MCP server.
    base_package(&dir.path().join("shell"), "shell");
    let error = SandboxSpec::package(dir.path().join("shell"))
        .with_mcp(McpServer::host("tools", 47813))
        .check(&client)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("shell has no MCP adapter"),
        "{error}"
    );
    // A package made for another daemon is refused before anything is
    // created.
    compiled_package(&dir.path().join("old"), "shell", "v0.54.1");
    let error = SandboxSpec::package(dir.path().join("old"))
        .launch(&client, "job-42", &Keys::new())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("package: the package was made for another daemon (v0.54.1)"),
        "{error}"
    );
    assert_eq!(sent(&daemon, "POST", "/sandbox").len(), 2);
}

/// The requests for `method path` of `daemon`, awaited until there are
/// `count`, or for five seconds.
async fn awaited(daemon: &FakeDaemon, method: &str, path: &str, count: usize) -> Vec<Recorded> {
    for _ in 0..50 {
        let requests = sent(daemon, method, path);
        if requests.len() >= count {
            return requests;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    sent(daemon, method, path)
}

#[tokio::test]
async fn a_launch_given_up_once_its_sandbox_exists_removes_it() {
    // The start answers after the launch is given up, as a timeout, a
    // `select!` or Ctrl-C gives it up.
    let slow_start = |reply: Reply| {
        launching(shell_export())
            .route(
                "POST",
                "/sandbox/job-42/start",
                reply.delayed(Duration::from_secs(2)),
            )
            .start()
    };
    for (reply, spec) in [
        // It started: the lease nobody takes closes its session.
        (
            Reply::json(200, &sandbox_json("job-42", "running")),
            SandboxSpec::agent(Agent::Shell),
        ),
        // It failed: kept by the spec, but nobody is left to look inside.
        (
            Reply::json(500, &json!({ "message": "the VM did not start" })),
            SandboxSpec::agent(Agent::Shell).keep_on_failure(),
        ),
    ] {
        let daemon = slow_start(reply);
        let client = connect(&daemon).await;
        let given_up = tokio::time::timeout(
            Duration::from_secs(1),
            spec.launch(&client, "job-42", &Keys::new()),
        )
        .await;
        assert!(given_up.is_err(), "the launch is given up during the start");
        assert_eq!(sent(&daemon, "POST", "/sandbox").len(), 1);
        let deleted = awaited(&daemon, "DELETE", "/sandbox/job-42", 1).await;
        assert_eq!(deleted.len(), 1, "the sandbox of the launch is removed");
        assert_eq!(deleted[0].query()["expected_id"], "11765de3");
    }
}
