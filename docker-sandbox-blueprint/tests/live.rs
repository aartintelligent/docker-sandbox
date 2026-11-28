//! Integration tests against the real `sandboxd` of the current user:
//! sandboxes launched from their spec.
//!
//! Disabled unless the `live` feature is enabled:
//!
//! ```sh
//! cargo test -p docker-sandbox-blueprint --features live --test live -- --test-threads=1
//! ```
//!
//! The tests launch `shell` with a mixin, a network refusal, a key and a
//! variable, then shut it down; `claude` with an MCP server of the host, its
//! token drawn at launch; `shell` again, its lease dropped; `shell` twice
//! from one cache of kit packages; `shell` given up three seconds into its
//! launch, which leaves no sandbox; and, when `DOCKER_SANDBOX_LIVE_PACKAGE`
//! names it, a package the `sbx` CLI compiled, launched as it is (`just
//! test-live` compiles the test mixin of `audit` for it). No agent is
//! ever started, and nothing serves the MCP server: the tests read what the
//! sandbox holds. The key is a dummy one, sent to the public echo service
//! `postman-echo.com`.
//!
//! Every resource created by these tests is removed afterwards, and the image
//! store is checked to be back to its initial state.

#![cfg(feature = "live")]
#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test by panicking"
)]

// The plumbing of the live tests of `docker-sandbox-kit`, shared rather than
// copied.
#[path = "../../docker-sandbox-kit/tests/live_support/mod.rs"]
mod live_support;

use docker_sandbox::Client;
use docker_sandbox::kit::KitPackage;
use docker_sandbox_blueprint::{KeySpec, Keys, Result, SandboxSpec};
use docker_sandbox_kit::{Agent, KitCache, KitDescriptor, LifecycleStep, McpServer, Mixin};
use live_support::{run, shell};

/// File the install command of the mixin writes.
const INSTALLED: &str = "/home/agent/docker-sandbox-blueprint.txt";

/// Host the spec refuses, which is never called.
const DENIED: &str = "docker-sandbox-blueprint.example.com";

/// Dummy value of the key, sent to the public echo service.
const DUMMY_KEY: &str = "docker-sandbox-blueprint-dummy-key";

/// The mixin of the tests: an install command.
fn tools() -> Mixin {
    let descriptor = KitDescriptor::mixin("docker-sandbox-blueprint live")
        .with_install(LifecycleStep::new(format!("echo installed > {INSTALLED}")).as_user("agent"));
    Mixin::new("/kits/docker-sandbox-blueprint-live", descriptor).unwrap()
}

/// Whether the daemon holds a network rule of the kits of the sandbox
/// `name` naming `host`.
async fn has_rule(client: &Client, name: &str, host: &str) -> Result<bool> {
    let prefix = format!("kit:{name}");
    Ok(client.policy().rules().await?.rules.iter().any(|rule| {
        rule.name.starts_with(&prefix) && rule.resources.iter().any(|resource| resource == host)
    }))
}

/// A shell sandbox launched from its spec, used through its lease, then
/// shut down with its rules.
#[tokio::test]
async fn a_spec_launches_its_sandbox_and_the_lease_removes_it() -> Result<()> {
    run("blueprint-shell", |client, name, _dir| async move {
        let spec = SandboxSpec::agent(Agent::Shell)
            .with_mixin(tools())
            .with_network_deny([DENIED])
            .with_key(KeySpec::for_hosts(
                "DOCKER_SANDBOX_KEY",
                ["postman-echo.com"],
            ))
            .with_env("DOCKER_SANDBOX_LOG", "debug");
        spec.check(&client).await?;
        let keys = Keys::new().with_key("DOCKER_SANDBOX_KEY", DUMMY_KEY);
        let lease = spec.launch(&client, &name, &keys).await?;
        let sandbox = lease.sandbox();
        assert_eq!(
            shell(sandbox, &format!("cat {INSTALLED}")).await?,
            "installed\n"
        );
        assert_eq!(
            shell(sandbox, "printf %s \"$DOCKER_SANDBOX_LOG\"").await?,
            "debug"
        );
        let placeholder = shell(sandbox, "printf %s \"$DOCKER_SANDBOX_KEY\"").await?;
        assert!(placeholder.starts_with("sbx-cs-"), "{placeholder}");
        let echoed = shell(
            sandbox,
            "curl -s -H \"x-api-key: $DOCKER_SANDBOX_KEY\" http://postman-echo.com/headers",
        )
        .await?;
        assert!(
            echoed.contains(DUMMY_KEY),
            "the key did not reach its host: {echoed}"
        );
        assert!(
            has_rule(&client, &name, DENIED).await?,
            "the rule of the spec"
        );
        let report = lease.report();
        assert!(
            report
                .kits
                .iter()
                .any(|kit| kit == "/kits/docker-sandbox-network")
        );
        assert_eq!(report.keys[0].hosts, ["postman-echo.com"]);

        lease.shutdown().await?;
        assert!(!client.sandbox(&name).exists().await?);
        assert!(
            !has_rule(&client, &name, DENIED).await?,
            "the rule outlived its sandbox"
        );
        Ok(())
    })
    .await
}

/// A claude sandbox with an MCP server of the host: its token, drawn at
/// launch, is in the variable Claude Code reads it from. The agent is never
/// started.
#[tokio::test]
async fn the_token_of_a_server_of_the_host_is_drawn_at_launch() -> Result<()> {
    run("blueprint-claude", |client, name, _dir| async move {
        let lease = SandboxSpec::agent(Agent::Claude)
            .with_mcp(McpServer::host("docker-sandbox-blueprint", 47813))
            .launch(&client, &name, &Keys::new())
            .await?;
        assert_eq!(
            lease.agent().and_then(|agent| agent.binary.as_deref()),
            Some("claude"),
            "the agent the kit declares"
        );
        let token = lease.mcp_token("docker-sandbox-blueprint").unwrap();
        assert_eq!(token.expose_secret().len(), 64);
        let held = shell(
            lease.sandbox(),
            "printf %s \"$DOCKER_SANDBOX_MCP_DOCKER_SANDBOX_BLUEPRINT_TOKEN\"",
        )
        .await?;
        assert_eq!(held, token.expose_secret());
        let registered = shell(
            lease.sandbox(),
            "python3 -c \"import json; s = json.load(open('/home/agent/.claude.json'))['mcpServers']; \
             print(s['docker-sandbox-blueprint']['url'])\"",
        )
        .await?;
        assert_eq!(registered, "http://host.docker.internal:47813/mcp\n");
        let server = &lease.report().mcp[0];
        assert_eq!((server.kind.as_str(), server.agent.as_str()), ("host", "claude"));
        lease.shutdown().await?;
        Ok(())
    })
    .await
}

/// A lease dropped without shutdown leaves its sandbox, which the program
/// removes.
#[tokio::test]
async fn a_dropped_lease_leaves_its_sandbox() -> Result<()> {
    run("blueprint-dropped", |client, name, _dir| async move {
        let lease = SandboxSpec::agent(Agent::Shell)
            .launch(&client, &name, &Keys::new())
            .await?;
        drop(lease);
        assert!(client.sandbox(&name).exists().await?, "the sandbox stays");
        Ok(())
    })
    .await
}

/// Two sandboxes of one spec, launched one after the other from one cache:
/// the second takes the kit the first forged.
#[tokio::test]
async fn the_sandboxes_of_a_spec_share_the_kit_of_a_cache() -> Result<()> {
    run("blueprint-cache", |client, name, _dir| async move {
        let cache = KitCache::new();
        let spec = SandboxSpec::agent(Agent::Shell).with_network_deny([DENIED]);
        for _ in 0..2 {
            let lease = spec
                .launch_with(&client, &name, &Keys::new(), &cache)
                .await?;
            assert!(
                has_rule(&client, &name, DENIED).await?,
                "the rule of the spec"
            );
            lease.shutdown().await?;
        }
        Ok(())
    })
    .await
}

/// A package the `sbx` CLI compiled, launched as it is: the test mixin of
/// `audit`, whose install command writes a file.
#[tokio::test]
async fn a_compiled_package_is_launched_as_it_is() -> Result<()> {
    let Ok(dir) = std::env::var("DOCKER_SANDBOX_LIVE_PACKAGE") else {
        eprintln!("skipped: DOCKER_SANDBOX_LIVE_PACKAGE names no compiled package");
        return Ok(());
    };
    run("blueprint-package", |client, name, _dir| async move {
        let package = KitPackage::open(&dir)?;
        let lease = SandboxSpec::package(&dir)
            .launch(&client, &name, &Keys::new())
            .await?;
        assert_eq!(lease.report().kits, package.kit_refs());
        assert_eq!(
            shell(lease.sandbox(), "cat /home/agent/audit-mixin.txt").await?,
            "audit-mixin-v3 installed\n"
        );
        lease.shutdown().await?;
        Ok(())
    })
    .await
}

/// A launch given up three seconds in, wherever it is then (forging,
/// creating or starting the sandbox), leaves no sandbox: the creation and
/// the launch remove what they made.
#[tokio::test]
async fn a_launch_given_up_leaves_no_sandbox() -> Result<()> {
    run("blueprint-given-up", |client, name, _dir| async move {
        let spec = SandboxSpec::agent(Agent::Shell).with_network_deny([DENIED]);
        let given_up = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            spec.launch(&client, &name, &Keys::new()),
        )
        .await;
        if let Ok(lease) = given_up {
            // Launched within three seconds: nothing was given up.
            lease?.shutdown().await?;
        }
        let sandbox = client.sandbox(&name);
        for _ in 0..60 {
            if !sandbox.exists().await? {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        assert!(!sandbox.exists().await?, "the launch left its sandbox");
        Ok(())
    })
    .await
}
