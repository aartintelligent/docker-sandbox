//! Sandbox specs checked against a fake daemon: the kit policy of the daemon
//! read, the mixins of a directory read, and nothing pulled or written.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

// The fake daemon of the tests of `docker-sandbox`, shared rather than copied.
#[path = "../../docker-sandbox/tests/support/mod.rs"]
mod support;

use docker_sandbox::sandbox::Workspace;
use docker_sandbox_blueprint::{Error, KeySpec, SandboxSpec};
use docker_sandbox_kit::{Agent, KitDescriptor, LifecycleStep, McpServer, Mixin};
use serde_json::{Value, json};
use support::{FakeDaemon, FakeDaemonBuilder, Reply, connect};

/// A daemon setting answer of `key` with `value`.
fn setting(key: &str, value: impl Into<Value>) -> Reply {
    let value = value.into();
    Reply::json(
        200,
        &json!({ "key": key, "value": value, "default": value, "source": "default", "type": "bool" }),
    )
}

/// A daemon whose kit policy allows local and unsigned kits.
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
}

/// A mixin of the program: an install command.
fn tools() -> Mixin {
    let descriptor = KitDescriptor::mixin("Acme tools")
        .with_install(LifecycleStep::new("make tools").as_user("agent"));
    Mixin::new("/kits/acme-tools", descriptor).unwrap()
}

/// A spec with a field of each kind.
fn whole(dir: &std::path::Path) -> SandboxSpec {
    SandboxSpec::agent(Agent::Claude)
        .with_arg("model", "claude-haiku-5-5")
        .with_mixin(tools())
        .with_mixin_dir(dir)
        .with_workspace("/srv/project")
        .with_mount(Workspace::new("/srv/docs").read_only())
        .with_network_allow(["api.example.com:443"])
        .with_network_deny(["*.pastebin.com"])
        .with_key(KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic"))
        .with_key(KeySpec::for_hosts("ACME_TOKEN", ["api.acme.example"]))
        .with_mcp(McpServer::host("tools", 47813))
        .with_mcp(
            McpServer::remote("docs", "https://mcp.example.com/mcp").with_bearer_env("ACME_TOKEN"),
        )
        .with_env("LOG", "debug")
        .with_cpus(2)
        .with_memory("4g")
}

#[tokio::test]
async fn specs_are_checked_without_pulling_anything() {
    let dir = tempfile::tempdir().unwrap();
    let descriptor = KitDescriptor::mixin("Local").with_network_deny(["*.example.org"]);
    std::fs::write(dir.path().join("spec.yaml"), descriptor.to_spec()).unwrap();
    let daemon = permissive().start();
    let client = connect(&daemon).await;
    whole(dir.path()).check(&client).await.unwrap();
    SandboxSpec::base("docker.io/sbx/shell:latest")
        .check(&client)
        .await
        .unwrap();
    // Read as a program in another language sends it.
    let spec: SandboxSpec = serde_json::from_value(json!({
        "agent": "claude",
        "mixins": [{ "dir": dir.path() }],
        "network": { "deny": ["*.pastebin.com"] },
        "mcp": [{ "kind": "stdio", "name": "files", "command": ["mcp-fs", "/work"] }]
    }))
    .unwrap();
    spec.check(&client).await.unwrap();
    assert!(
        !daemon
            .requests()
            .iter()
            .any(|r| r.path().starts_with("/docker/images") || r.path().starts_with("/sandbox")),
        "nothing is pulled or created"
    );
}

#[tokio::test]
async fn the_kit_policy_of_the_daemon_and_the_forge_refuse_a_spec() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = permissive()
        .route(
            "GET",
            "/daemon/settings/kit.allowLocalKits",
            setting("kit.allowLocalKits", false),
        )
        .start();
    let client = connect(&daemon).await;
    // The network rules are a mixin, which a daemon without local kits refuses.
    let error = SandboxSpec::agent(Agent::Shell)
        .with_network_deny(["*.pastebin.com"])
        .check(&client)
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Kit(ref e) if e.to_string().contains("kit.allowLocalKits is off")),
        "{error}"
    );
    SandboxSpec::agent(Agent::Shell)
        .check(&client)
        .await
        .unwrap();

    // A directory without a mixin, and an argument of a kit not composed.
    let daemon = permissive().start();
    let client = connect(&daemon).await;
    let error = SandboxSpec::agent(Agent::Shell)
        .with_mixin_dir(dir.path().join("missing"))
        .check(&client)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Kit(_)), "{error}");
    let error = SandboxSpec::agent(Agent::Shell)
        .with_arg("acme.model", "m")
        .check(&client)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Kit(_)), "{error}");

    // A daemon whose settings cannot be read.
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/daemon/settings/kit.requireSignature",
            Reply::json(500, &json!({ "message": "settings unavailable" })),
        )
        .start();
    let error = SandboxSpec::agent(Agent::Shell)
        .check(&connect(&daemon).await)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Kit(_)), "{error}");
}

#[tokio::test]
async fn a_rule_a_spec_breaks_is_named_before_the_daemon_is_asked() {
    let daemon = permissive().start();
    let client = connect(&daemon).await;
    let asked = daemon.requests().len();
    let error = SandboxSpec::agent(Agent::Claude)
        .with_network_deny(["https://api.example.com"])
        .check(&client)
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::InvalidSpec(ref m) if m.starts_with("network.deny: ")),
        "{error}"
    );
    assert!(
        error.to_string().starts_with("invalid sandbox spec: "),
        "{error}"
    );
    assert_eq!(daemon.requests().len(), asked, "the daemon is not asked");
}
