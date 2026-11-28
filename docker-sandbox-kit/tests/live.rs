//! Integration tests against the real `sandboxd` of the current user: kit
//! packages forged without the CLI.
//!
//! Disabled unless the `live` feature is enabled:
//!
//! ```sh
//! cargo test -p docker-sandbox-kit --features live --test live -- --test-threads=1
//! ```
//!
//! The tests create sandboxes from packages forged through the socket alone:
//! `shell` and `claude` rebuilt from their published image, `claude` with two of
//! its arguments, MCP servers registered in Claude Code by a mixin, and a
//! forged mixin; `copilot` and `opencode` with MCP servers registered beside
//! the gateway their kit registers; `shell` and a mixin kept by a cache in a
//! directory and taken from there. They need no package and no `sbx` CLI.
//! No agent is ever started: resolved as the CLI resolves it,
//! the kit is authorized the credential the daemon holds for `anthropic`,
//! which the proxy would write into requests to the hosts of Anthropic, and
//! the tests send none. `proxied_keys_*` sends a dummy key to the public echo
//! service `postman-echo.com`.
//!
//! Every resource created by these tests is removed afterwards, and the image
//! store is checked to be back to its initial state.

#![cfg(all(feature = "live", feature = "forge"))]
#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test by panicking"
)]

mod live_support;

use docker_sandbox::Client;
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::sandbox::{ProxiedKey, Sandbox};
use docker_sandbox_kit::{
    Agent, KitCache, KitDescriptor, KitForge, LifecycleStep, McpServer, Mixin, Result,
};
use live_support::{run, shell};

/// File the install command of the forged mixin writes.
const INSTALLED: &str = "/home/agent/docker-sandbox-forged.txt";

/// File the startup command of the forged mixin writes.
const STARTED: &str = "/tmp/docker-sandbox-forged-startup.txt";

/// Host the forged mixin allows, which is never called.
const ALLOWED: &str = "docker-sandbox-live.example.com:443";

/// Dummy value of the proxied key, sent to the public echo service.
const DUMMY_KEY: &str = "docker-sandbox-live-dummy-key";

/// The mixin of the tests: an install command, a startup command run in
/// the background, and a network rule.
fn mixin() -> Mixin {
    let descriptor = KitDescriptor::mixin("docker-sandbox live")
        .with_description("Forged by the live tests of docker-sandbox")
        .with_network_allow([ALLOWED])
        .with_install(LifecycleStep::new(format!("echo installed > {INSTALLED}")).as_user("agent"))
        .with_startup(LifecycleStep::new(format!("echo started > {STARTED}")).in_background());
    Mixin::new("/kits/docker-sandbox-live", descriptor).unwrap()
}

/// Checks that the commands of the forged mixin ran in `sandbox`.
async fn assert_mixin_ran(sandbox: &Sandbox) -> Result<()> {
    assert_eq!(
        shell(sandbox, &format!("cat {INSTALLED}")).await?,
        "installed\n"
    );
    // The startup command runs in the background: wait for its file.
    let started =
        format!("for i in $(seq 20); do [ -e {STARTED} ] && break; sleep 0.5; done; cat {STARTED}");
    assert_eq!(shell(sandbox, &started).await?, "started\n");
    Ok(())
}

/// Whether the daemon holds the network rule the mixin gave `name`.
async fn has_kit_rule(client: &Client, name: &str) -> Result<bool> {
    let rule = format!("kit:{name}");
    Ok(client.policy().rules().await?.rules.iter().any(|r| {
        r.name == rule
            && r.resources
                .iter()
                .any(|host| host.contains("docker-sandbox-live"))
    }))
}

/// The shell kit rebuilt from its image, with a forged mixin.
#[tokio::test]
async fn forged_package_on_the_published_shell_kit() -> Result<()> {
    run("forged-shell", |client, name, dir| async move {
        let package = KitForge::new(client.clone())
            .with_base("docker.io/sbx/shell:latest")
            .with_mixin(mixin())
            .forge(dir.join("shell+forged"))
            .await?;
        assert_eq!(package.compiled_with(), None, "no sbx compiled it");
        assert_eq!(
            package.compiled_api(),
            Some(client.health().api_version.as_str())
        );
        let created = package.create(&client, &name).await?;
        assert_eq!(created.name, name);
        assert!(created.withheld.is_empty(), "{:?}", created.withheld);

        let sandbox = client.sandbox(&name);
        {
            let _session = sandbox.ensure_running().await?;
            assert_mixin_ran(&sandbox).await?;
            assert!(
                has_kit_rule(&client, &name).await?,
                "the rule of the mixin is missing"
            );
        }
        sandbox.force_delete().await?;
        assert!(
            !has_kit_rule(&client, &name).await?,
            "the rule outlived its sandbox"
        );
        Ok(())
    })
    .await
}

/// The shell kit and a mixin kept by a cache in a directory: a second cache
/// on that directory takes the package from there, and a sandbox is created
/// from it.
#[tokio::test]
async fn a_cache_keeps_the_package_of_a_composition() -> Result<()> {
    run("forged-cache", |client, name, dir| async move {
        let forge = KitForge::agent(client.clone(), Agent::Shell).with_mixin(mixin());
        let kept = KitCache::new().with_dir(&dir).package(&forge).await?;
        let reused = KitCache::new().with_dir(&dir).package(&forge).await?;
        assert_eq!(reused.dir(), kept.dir(), "taken from the directory");
        assert!(reused.suits(client.health()));
        let created = reused.create(&client, &name).await?;
        assert_eq!(created.name, name);
        let sandbox = client.sandbox(&name);
        {
            let _session = sandbox.ensure_running().await?;
            assert_mixin_ran(&sandbox).await?;
        }
        Ok(())
    })
    .await
}

/// A stdio server for the agent, registered and never started.
fn files() -> McpServer {
    McpServer::stdio("docker-sandbox-files", "mcp-fs").with_args(["/work", "it's"])
}

/// Creates a sandbox of the kit of `agent` with a server of the host, a remote
/// server behind a token and a stdio server registered by a mixin, starts it,
/// and checks that the kit kept them beside its gateway in the object `key`
/// of the configuration file `config` of the agent. The agent is never
/// started.
async fn mcp_beside_the_gateway(agent: Agent, config: &str, key: &str) -> Result<()> {
    run(
        &format!("forged-mcp-{agent}"),
        |client, name, _dir| async move {
            let tools = McpServer::host("docker-sandbox-live", 47813);
            let docs = McpServer::remote("docker-sandbox-docs", "https://mcp.example.com/mcp")
                .with_bearer_env("DOCKER_SANDBOX_DOCS_TOKEN");
            let package = KitForge::agent(client.clone(), agent)
                .with_mixin(Mixin::mcp(agent, [tools.clone(), docs, files()])?)
                .build()
                .await?;
            let request = package
                .request(&name)?
                .with_env(tools.token_env().unwrap(), "docker-sandbox-live-token")
                .with_env("DOCKER_SANDBOX_DOCS_TOKEN", "docker-sandbox-docs-token");
            package.create_with(&client, &request).await?;
            let sandbox = client.sandbox(&name);
            // The kit merges its gateway into the file at each start.
            let _session = sandbox.ensure_running().await?;
            let servers = shell(&sandbox, &format!("jq -c '.{key}' {config}")).await?;
            let servers: serde_json::Value = serde_json::from_str(&servers).unwrap();
            assert!(
                servers.get("mcp-gateway").is_some(),
                "the kit registers its gateway"
            );
            let (bearer, docs_bearer) = if agent == Agent::OpenCode {
                (
                    "Bearer {env:DOCKER_SANDBOX_MCP_DOCKER_SANDBOX_LIVE_TOKEN}",
                    "Bearer {env:DOCKER_SANDBOX_DOCS_TOKEN}",
                )
            } else {
                (
                    "Bearer ${DOCKER_SANDBOX_MCP_DOCKER_SANDBOX_LIVE_TOKEN}",
                    "Bearer ${DOCKER_SANDBOX_DOCS_TOKEN}",
                )
            };
            let read = |server: &str, field: &str| servers[server][field].clone();
            assert_eq!(
                read("docker-sandbox-live", "url"),
                "http://host.docker.internal:47813/mcp"
            );
            assert_eq!(
                read("docker-sandbox-live", "headers")["Authorization"],
                bearer
            );
            assert_eq!(
                read("docker-sandbox-docs", "headers")["Authorization"],
                docs_bearer
            );
            assert_eq!(read("docker-sandbox-files", "type"), "local");
            if agent == Agent::OpenCode {
                assert_eq!(
                    read("docker-sandbox-files", "command"),
                    serde_json::json!(["mcp-fs", "/work", "it's"])
                );
            } else {
                assert_eq!(read("docker-sandbox-files", "command"), "mcp-fs");
                assert_eq!(
                    read("docker-sandbox-files", "args"),
                    serde_json::json!(["/work", "it's"])
                );
            }
            Ok(())
        },
    )
    .await
}

/// The copilot kit rebuilt from its image, with MCP servers registered by a
/// mixin in `~/.copilot/mcp-config.json`, beside the gateway of the kit.
#[tokio::test]
async fn copilot_keeps_the_servers_of_a_mixin() -> Result<()> {
    mcp_beside_the_gateway(
        Agent::Copilot,
        "/home/agent/.copilot/mcp-config.json",
        "mcpServers",
    )
    .await
}

/// The opencode kit rebuilt from its image, with MCP servers registered by a
/// mixin in `~/.config/opencode/opencode.json`, beside the gateway of the
/// kit.
#[tokio::test]
async fn opencode_keeps_the_servers_of_a_mixin() -> Result<()> {
    mcp_beside_the_gateway(
        Agent::OpenCode,
        "/home/agent/.config/opencode/opencode.json",
        "mcp",
    )
    .await
}

/// The claude kit rebuilt from its image with two of its arguments, MCP
/// servers registered by a mixin and a forged mixin, built in memory. The
/// agent is never started, and Claude Code never connects to the servers:
/// the test reads what the install commands of the mixin registered.
#[tokio::test]
async fn claude_rebuilt_with_its_arguments() -> Result<()> {
    run("forged-claude", |client, name, _dir| async move {
        let tools = McpServer::host("docker-sandbox-live", 47813);
        let package = KitForge::agent(client.clone(), Agent::Claude)
            .with_arg("model", "claude-haiku-5-5")
            .with_arg("reasoningEffort", "low")
            .with_mixin(Mixin::mcp(Agent::Claude, [tools.clone(), files()])?)
            .with_mixin(mixin())
            .build()
            .await?;
        // The token of the server is given at creation, a dummy one.
        let request = package
            .request(&name)?
            .with_env(tools.token_env().unwrap(), "docker-sandbox-live-token");
        let created = package.create_with(&client, &request).await?;
        // Resolved as the CLI resolves it, the kit is authorized the
        // credential it declares, without a bindings file: the daemon
        // withholds nothing. With its effective capabilities in another
        // order, it would withhold `anthropic`.
        assert!(
            created.withheld.is_empty(),
            "withheld {:?}",
            created.withheld
        );

        let sandbox = client.sandbox(&name);
        let _session = sandbox.ensure_running().await?;
        assert_mixin_ran(&sandbox).await?;
        let variables = shell(&sandbox, "echo \"$CLAUDE_MODEL $CLAUDE_REASONING_EFFORT\"").await?;
        assert_eq!(variables, "claude-haiku-5-5 low\n");
        // Only the two fields the install commands of the kit write from
        // the arguments are read from the settings.
        let settings = shell(
            &sandbox,
            "python3 -c \"import json; d = json.load(open('/home/agent/.claude/settings.json')); \
             print(d.get('model'), d.get('effortLevel'))\"",
        )
        .await?;
        assert_eq!(settings, "claude-haiku-5-5 low\n");
        // Only the servers of the mixin are read from the configuration of
        // Claude Code, which `claude mcp add` wrote at install time: the
        // token is a reference to its variable, which Claude Code reads when
        // it connects.
        let registered = shell(
            &sandbox,
            "python3 -c \"import json; s = json.load(open('/home/agent/.claude.json'))['mcpServers']; \
             t = s['docker-sandbox-live']; f = s['docker-sandbox-files']; \
             print(t['type'], t['url'], t['headers']['Authorization']); \
             print(f['type'], f['command'], *f['args'])\"",
        )
        .await?;
        assert_eq!(
            registered,
            "http http://host.docker.internal:47813/mcp \
             Bearer ${DOCKER_SANDBOX_MCP_DOCKER_SANDBOX_LIVE_TOKEN}\n\
             stdio mcp-fs /work it's\n"
        );
        assert!(
            sandbox
                .exec(&ExecSpec::new([
                    "test",
                    "-s",
                    "/home/agent/.claude/CLAUDE.md"
                ]))
                .await?
                .success(),
            "the agent context of the kit is missing"
        );
        Ok(())
    })
    .await
}

/// A key given through the proxy alone: the sandbox holds its
/// placeholder, and the proxy writes the key in its place in a request to
/// its host, the public echo service, which answers with the headers it
/// received. The value is a dummy.
#[tokio::test]
async fn proxied_keys_reach_their_host_through_the_proxy() -> Result<()> {
    run("proxied-key", |client, name, dir| async move {
        let package = KitForge::new(client.clone())
            .with_base("docker.io/sbx/shell:latest")
            .forge(dir.join("shell"))
            .await?;
        let key = ProxiedKey::new("DOCKER_SANDBOX_LIVE_KEY", ["postman-echo.com"], DUMMY_KEY)?;
        let request = package.request(&name)?.with_proxied_key(&key);
        package.create_with(&client, &request).await?;

        let sandbox = client.sandbox(&name);
        let _session = sandbox.ensure_running().await?;
        let outcome = sandbox
            .push_proxied_keys(std::slice::from_ref(&key))
            .await?;
        assert!(outcome.withheld.is_empty(), "{:?}", outcome.withheld);
        assert_eq!(
            shell(&sandbox, "printf %s \"$DOCKER_SANDBOX_LIVE_KEY\"").await?,
            key.placeholder(),
            "the sandbox holds the placeholder"
        );
        let echoed = shell(
            &sandbox,
            "curl -s -H \"x-api-key: $DOCKER_SANDBOX_LIVE_KEY\" http://postman-echo.com/headers",
        )
        .await?;
        assert!(
            echoed.contains(DUMMY_KEY),
            "the key did not reach its host: {echoed}"
        );
        assert!(!echoed.contains(key.placeholder()), "{echoed}");
        Ok(())
    })
    .await
}
