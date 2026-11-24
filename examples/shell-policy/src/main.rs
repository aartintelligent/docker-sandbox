//! The policy beyond the hosts of one sandbox (see `shell-network` for those):
//! global rules, protocols, rules removed by identifier, failures the daemon
//! reports inside a success, the reload of the policy, permissions on host
//! paths, and the stream of approval prompts.
//!
//! - A **global** rule applies to every sandbox. The example adds one, a deny
//!   of TCP to a host of the reserved `.invalid` domain, which no real
//!   traffic reaches, checks it, then removes it by identifier: whatever
//!   happens, the global policy ends as it began.
//! - The daemon answers a batch of rule changes with HTTP 200 and reports a
//!   failed change inside: `RuleChangeResult::is_applied` and `into_result`
//!   tell it.
//! - A rule scoped to a sandbox, for a host or for an HTTP method and path, is
//!   removed through that sandbox: the global removal does not find it.
//! - A host path permission (`Sandbox::allow_host_path`) is a grant, listed
//!   with the runtime mounts of the sandbox, not a mount.
//! - Approval prompts (`Prompts::subscribe`) start with a `synced` line; an
//!   unknown prompt cannot be answered.
//!
//! ```sh
//! just example shell-policy
//! ```

use anyhow::{Context, Result, ensure};
use docker_sandbox::Client;
use docker_sandbox::policy::{NetworkCheck, Policy, Protocol, RuleChange};
use docker_sandbox::sandbox::Sandbox;
use example_common::{connect, kit, sandbox};

/// Host of the global rule, in a domain reserved to never resolve.
const GLOBAL_HOST: &str = "example-global-rule.invalid";

/// Host allowed for the sandbox only.
const SANDBOX_HOST: &str = "example-sandbox-rule.invalid";

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let policy = client.policy();
    let rules = policy.rules().await?;
    let global = rules
        .rules
        .iter()
        .filter(|rule| rule.scope.as_deref() == Some("global"))
        .count();
    println!(
        "{} rules, {global} of them global, governance {}",
        rules.rules.len(),
        if rules.governance_active {
            "active"
        } else {
            "inactive"
        }
    );

    global_rule(&policy).await?;
    failed_change(&policy).await?;
    policy.refresh().await?;
    println!("policy reloaded");

    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("policy");
    sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        sandbox_rule(&policy, &sandbox).await?;
        host_path(&sandbox).await?;
        Ok(())
    })
    .await?;
    prompts(&client).await
}

/// Adds a global deny of TCP to [`GLOBAL_HOST`], checks it, and removes it by
/// identifier, also when the check fails.
async fn global_rule(policy: &Policy) -> Result<()> {
    let change = RuleChange::deny([GLOBAL_HOST]).with_protocols([Protocol::Tcp]);
    let results = policy.apply_all(&[change]).await?;
    let created: Vec<_> = results.iter().flat_map(|result| &result.created).collect();
    let ids: Vec<String> = created.iter().map(|rule| rule.rule_id.clone()).collect();
    for rule in &created {
        println!(
            "global rule {} added: deny {} over {}",
            rule.rule_id,
            rule.resource,
            rule.protocols.join(", ")
        );
    }
    let checked = async {
        let check = policy
            .check(&NetworkCheck::host(format!("{GLOBAL_HOST}:443")))
            .await?;
        ensure!(!check.allowed, "{GLOBAL_HOST} is allowed");
        println!(
            "check of {GLOBAL_HOST}:443: denied by {}",
            check.rule.as_deref().unwrap_or("a rule")
        );
        anyhow::Ok(())
    }
    .await;
    for id in &ids {
        policy.remove_rule(id).await?;
    }
    println!("global rule removed by identifier");
    checked
}

/// Asks for the removal of a rule that does not exist: the daemon answers
/// with a success that holds the failure.
async fn failed_change(policy: &Policy) -> Result<()> {
    let results = policy
        .apply(&[RuleChange::remove_rule("example-no-such-rule")])
        .await?;
    let result = results.into_iter().next().context("no result")?;
    ensure!(!result.is_applied(), "a missing rule was removed");
    let error = result
        .into_result()
        .err()
        .context("the failed change is a success")?;
    println!("removal of a missing rule: HTTP 200, the change failed: {error}");
    Ok(())
}

/// Allows [`SANDBOX_HOST`] for `sandbox`, lists the rule, shows that the
/// global removal does not find it, then removes it through the sandbox.
async fn sandbox_rule(policy: &Policy, sandbox: &Sandbox) -> Result<()> {
    let result = sandbox.allow_hosts([SANDBOX_HOST]).await?;
    let id = result
        .created
        .first()
        .map(|rule| rule.rule_id.clone())
        .context("no rule created")?;
    let listed = sandbox.network_rules().await?;
    ensure!(
        listed
            .iter()
            .any(|rule| rule.id == id && rule.is_for_sandbox(sandbox.name())),
        "the rule is not listed for the sandbox"
    );
    println!("sandbox rule {id} added: allow {SANDBOX_HOST}");
    let global = policy.remove_rule(&id).await;
    ensure!(global.is_err(), "the global removal found a sandbox rule");
    println!(
        "its global removal fails: {}",
        global
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default()
    );
    sandbox.remove_network_rule(&id).await?.into_result()?;
    println!("removed through the sandbox");

    // An HTTP rule, narrowed to a method and a path, for this sandbox only.
    let http = RuleChange::allow_http([SANDBOX_HOST])
        .with_methods(["GET"])
        .with_path("/v1/**")
        .for_sandbox(sandbox.name());
    let results = policy.apply_all(&[http]).await?;
    let ids: Vec<String> = results
        .iter()
        .flat_map(|result| &result.created)
        .map(|rule| rule.rule_id.clone())
        .collect();
    let rule = sandbox
        .network_rules()
        .await?
        .into_iter()
        .find(|rule| ids.contains(&rule.id))
        .context("the HTTP rule is not listed")?;
    let targets: Vec<String> = rule
        .http_targets
        .iter()
        .map(|target| {
            format!(
                "{}{}",
                target.host,
                target.path.as_deref().unwrap_or_default()
            )
        })
        .collect();
    println!(
        "HTTP rule {} added: {:?} {} {}",
        rule.id,
        rule.decision,
        rule.methods.join(","),
        targets.join(", ")
    );
    for id in &ids {
        sandbox.remove_network_rule(id).await?.into_result()?;
    }
    println!("HTTP rule removed through the sandbox");
    Ok(())
}

/// Grants `sandbox` a host directory made for the run, then revokes it. The
/// path must exist on the host: the daemon refuses another one.
async fn host_path(sandbox: &Sandbox) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("example-host-path-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir
        .canonicalize()?
        .to_str()
        .context("the temporary directory is not UTF-8")?
        .to_owned();
    let outcome = grant_and_revoke(sandbox, &path).await;
    let _ = std::fs::remove_dir(&dir);
    outcome
}

/// Grants `sandbox` the host path `path`, then revokes it, reading the runtime
/// mounts of the sandbox after each.
async fn grant_and_revoke(sandbox: &Sandbox, path: &str) -> Result<()> {
    let granted = || async {
        let info = sandbox.inspect().await?;
        anyhow::Ok(
            info.runtime_mounts
                .iter()
                .any(|mount| mount.host_path == path),
        )
    };
    sandbox.allow_host_path(path).await?;
    println!(
        "host path granted, listed with the runtime mounts: {}",
        granted().await?
    );
    sandbox.revoke_host_path(path).await?;
    ensure!(!granted().await?, "the grant is still listed");
    println!("revoked: no longer listed");
    Ok(())
}

/// Reads the prompt stream until its `synced` line, and answers a prompt that
/// does not exist.
async fn prompts(client: &Client) -> Result<()> {
    let prompts = client.prompts();
    let mut stream = prompts.subscribe().await?;
    let mut pending = 0;
    loop {
        let event = stream.next().await.context("the prompt stream ended")??;
        if event.is_synced() {
            break;
        }
        pending += 1;
    }
    println!("prompt stream synced, {pending} prompts pending");
    let error = prompts
        .respond("example-no-such-prompt", "allow")
        .await
        .err()
        .context("an unknown prompt was answered")?;
    ensure!(error.is_not_found(), "unexpected error: {error}");
    println!("an unknown prompt cannot be answered: not found");
    Ok(())
}
