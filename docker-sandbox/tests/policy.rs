//! Network policy and MCP operations against a fake daemon.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

mod support;

use docker_sandbox::Error;
use docker_sandbox::mcp::McpServerCheck;
use docker_sandbox::policy::{Decision, NetworkCheck, RuleChange};
use serde_json::{Value, json};
use support::{FakeDaemon, Reply, connect};

/// A global rule and a rule of the sandbox `job-42`.
fn rules() -> Value {
    json!({
        "governance_active": false,
        "rules": [
            { "id": "default-allow-all", "name": "default-allow-all", "decision": "allow", "actions": ["net:connect:tcp"],
              "resources": ["**"], "scope": "global", "applies_to": "all", "editable": true },
            { "id": "r1", "name": "r1", "decision": "deny", "actions": ["net:connect:tcp", "net:connect:udp"],
              "resources": ["tracker.example.com"], "scope": "sandbox:job-42", "sandbox_id": "job-42" }
        ]
    })
}

/// The outcome of one change.
fn outcome(action: &str) -> Reply {
    Reply::json(
        200,
        &json!({ "results": [{ "action": action, "resources": ["api.github.com"], "sandbox_id": "job-42",
                                "created": [{ "rule_id": "r2", "resource": "api.github.com", "protocols": ["tcp"] }] }] }),
    )
}

#[tokio::test]
async fn sandbox_rules_are_filtered_from_the_policy() {
    let daemon = FakeDaemon::builder()
        .route("GET", "/policy/network/rules", Reply::json(200, &rules()))
        .start();
    let client = connect(&daemon).await;

    let all = client.policy().rules().await.unwrap();
    assert_eq!(all.rules.len(), 2);
    assert_eq!(all.rules[0].decision, Decision::Allow);

    let scoped = client.sandbox("job-42").network_rules().await.unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].decision, Decision::Deny);
    assert_eq!(
        daemon.requests()[0].query()["type"],
        "all",
        "every rule type is listed"
    );
}

#[tokio::test]
async fn sandbox_helpers_scope_their_changes() {
    let daemon = FakeDaemon::builder()
        .route("POST", "/policy/network/rules", outcome("allow"))
        .start();
    let client = connect(&daemon).await;

    let result = client
        .sandbox("job-42")
        .allow_hosts(["api.github.com"])
        .await
        .unwrap();
    assert_eq!(result.created[0].rule_id, "r2");
    assert_eq!(
        daemon.request("POST", "/policy/network/rules").json(),
        json!({ "actions": [{ "action": "allow", "protocols": ["tcp"], "resources": ["api.github.com"], "sandbox_id": "job-42" }] })
    );
}

#[tokio::test]
async fn changes_are_applied_in_one_request() {
    let daemon = FakeDaemon::builder()
        .route("POST", "/policy/network/rules", outcome("http-allow"))
        .start();
    let client = connect(&daemon).await;

    let changes = [
        RuleChange::allow_http(["api.example.com"])
            .with_methods(["GET"])
            .with_path("/v1/**")
            .for_sandbox("job-42"),
        RuleChange::remove(["old.example.com"]).for_sandbox("job-42"),
    ];
    client.policy().apply(&changes).await.unwrap();
    let body = daemon.request("POST", "/policy/network/rules").json();
    assert_eq!(body["actions"][0]["action"], "http-allow");
    assert_eq!(body["actions"][1]["action"], "remove-resource");
}

#[tokio::test]
async fn checks_logs_and_setup_are_read() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/policy/network/check",
            Reply::json(200, &json!({ "allowed": true, "rule": "default-allow-all", "resource_value": "api.anthropic.com:443" })),
        )
        .route(
            "GET",
            "/policy/network/log",
            Reply::json(
                200,
                &json!({ "allowed_hosts": [
                    { "host": "api.anthropic.com:443", "vm_name": "job-42", "count_since": 3 },
                    { "host": "pypi.org:443", "vm_name": "other", "count_since": 1 }
                ], "blocked_hosts": [] }),
            ),
        )
        .route(
            "GET",
            "/policy/network/setup",
            Reply::json(200, &json!({ "needed": false })),
        )
        .route(
            "GET",
            "/policy/network/profiles",
            Reply::json(200, &json!({ "profiles": [] })),
        )
        .route("POST", "/policy/refresh", Reply::empty(204))
        .start();
    let client = connect(&daemon).await;
    let policy = client.policy();

    let check = policy
        .check(&NetworkCheck::http(
            "POST",
            "https://api.anthropic.com/v1/messages",
        ))
        .await
        .unwrap();
    assert!(check.allowed);
    assert_eq!(
        daemon.request("POST", "/policy/network/check").json(),
        json!({ "type": "http", "target": "https://api.anthropic.com/v1/messages", "method": "POST" })
    );

    let log = policy.log().await.unwrap().for_sandbox("job-42");
    assert_eq!(log.allowed_hosts.len(), 1);
    assert_eq!(log.allowed_hosts[0].count_since, 3);

    assert!(!policy.setup_needed().await.unwrap());
    assert!(policy.profiles().await.unwrap().is_empty());
    policy.refresh().await.unwrap();
}

#[tokio::test]
async fn mcp_servers_are_loaded_and_unloaded() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/mcp/gateway/servers",
            Reply::json(200, &json!({ "sandbox": "job-42", "server": "docs" })),
        )
        .route(
            "DELETE",
            "/sandbox/job-42/mcp/gateway/servers/docs",
            Reply::empty(204),
        )
        .route(
            "GET",
            "/mcp/gateway-mode",
            Reply::json(
                200,
                &json!({ "decision": "local", "gateway_url": "none", "reason": "local" }),
            ),
        )
        .start();
    let client = connect(&daemon).await;
    let sandbox = client.sandbox("job-42");

    let loaded = sandbox.load_mcp_server("docs").await.unwrap();
    assert_eq!(loaded.server, "docs");
    assert_eq!(
        daemon
            .request("POST", "/sandbox/job-42/mcp/gateway/servers")
            .json(),
        json!({ "server": "docs" })
    );
    sandbox.unload_mcp_server("docs").await.unwrap();
    assert_eq!(client.mcp().gateway_mode().await.unwrap().decision, "local");
}

#[tokio::test]
async fn a_static_gateway_lists_its_servers() {
    // The answer measured for a sandbox created with `--static-mcp audit-echo`
    // (`static-gateway` of the `coverage` suite); without servers loaded, the
    // daemon leaves `servers` out.
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/sandbox/job-42/mcp/gateway",
            Reply::json(
                200,
                &json!({
                    "port": 44051,
                    "servers": ["audit-echo"],
                    "state": "ready",
                    "static": true,
                    "url": "http://mcp-gateway.docker.internal/mcp"
                }),
            ),
        )
        .route(
            "GET",
            "/sandbox/job-43/mcp/gateway",
            Reply::json(
                200,
                &json!({ "port": 32965, "state": "ready", "static": false }),
            ),
        )
        .start();
    let client = connect(&daemon).await;

    let gateway = client.sandbox("job-42").mcp_gateway().await.unwrap();
    assert!(gateway.static_servers);
    assert_eq!(gateway.servers, ["audit-echo"]);
    assert_eq!(
        gateway.url.as_deref(),
        Some("http://mcp-gateway.docker.internal/mcp")
    );
    let gateway = client.sandbox("job-43").mcp_gateway().await.unwrap();
    assert!(!gateway.static_servers);
    assert!(gateway.servers.is_empty());
}

#[tokio::test]
async fn mcp_servers_are_checked_with_the_fields_the_daemon_needs() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/mcp/registration/check",
            Reply::json(200, &json!({ "allowed": true, "name": "docs" })),
        )
        .start();
    let client = connect(&daemon).await;

    let check = client
        .mcp()
        .check_registration(&McpServerCheck::local("docs", ["npx", "docs-mcp"]))
        .await
        .unwrap();
    assert!(check.allowed);
    assert_eq!(
        daemon.request("POST", "/mcp/registration/check").json(),
        json!({ "spec": { "Name": "docs", "Type": "local", "Command": ["npx", "docs-mcp"] } })
    );
    assert_eq!(
        serde_json::to_value(McpServerCheck::remote("s", "https://mcp.example.com/mcp")).unwrap(),
        json!({ "Name": "s", "Type": "remote", "URL": "https://mcp.example.com/mcp" })
    );
}

#[tokio::test]
async fn failures_reported_inside_results_are_errors_for_the_helpers() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/policy/network/rules",
            Reply::json(
                200,
                &json!({ "results": [{ "action": "remove-id", "id": "zz", "error": "remove-id: not found: rule \"zz\"" }] }),
            ),
        )
        .start();
    let client = connect(&daemon).await;
    let policy = client.policy();

    let results = policy
        .apply(&[RuleChange::remove_rule("zz")])
        .await
        .unwrap();
    assert!(!results[0].is_applied(), "apply reports the outcome as is");
    assert!(matches!(
        policy.apply_all(&[RuleChange::remove_rule("zz")]).await,
        Err(Error::Failed { .. })
    ));
    assert!(matches!(
        policy.remove_rule("zz").await,
        Err(Error::Failed { ref message, .. }) if message.contains("not found")
    ));
    assert!(matches!(
        client.sandbox("job-42").remove_hosts(["zz"]).await,
        Err(Error::Failed { .. })
    ));
    assert_eq!(
        daemon.requests()[0].json(),
        json!({ "actions": [{ "action": "remove-id", "id": "zz" }] })
    );
}

#[tokio::test]
async fn sandbox_rules_are_denied_and_removed_by_identifier() {
    let daemon = FakeDaemon::builder()
        .route("POST", "/policy/network/rules", outcome("deny"))
        .start();
    let client = connect(&daemon).await;
    let sandbox = client.sandbox("job-42");

    sandbox.deny_hosts(["tracker.example.com"]).await.unwrap();
    sandbox.remove_network_rule("r2").await.unwrap();
    let actions: Vec<Value> = daemon
        .requests()
        .iter()
        .map(|r| r.json()["actions"][0].clone())
        .collect();
    assert_eq!(
        actions,
        [
            json!({ "action": "deny", "protocols": ["tcp", "udp"], "resources": ["tracker.example.com"], "sandbox_id": "job-42" }),
            json!({ "action": "remove-id", "id": "r2", "sandbox_id": "job-42" }),
        ]
    );
}

#[tokio::test]
async fn a_change_without_outcome_is_a_failure() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/policy/network/rules",
            Reply::json(200, &json!({ "results": [] })),
        )
        .start();
    let client = connect(&daemon).await;

    let error = client
        .sandbox("job-42")
        .allow_hosts(["api.github.com"])
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Failed { ref message, .. } if message == "the daemon returned no outcome"),
        "{error}"
    );
}

#[test]
fn host_checks_and_decisions_use_the_daemon_spelling() {
    assert_eq!(
        serde_json::to_value(NetworkCheck::host("api.github.com:443")).unwrap(),
        json!({ "type": "network", "target": "api.github.com:443" })
    );
    for (decision, wire) in [
        (Decision::Allow, "allow"),
        (Decision::Deny, "deny"),
        (Decision::Other("ask".into()), "ask"),
    ] {
        assert_eq!(String::from(decision.clone()), wire);
        assert_eq!(Decision::from(wire.to_owned()), decision);
    }
}
