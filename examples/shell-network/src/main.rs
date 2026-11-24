//! The network policy of a shell sandbox: hosts allowed and denied for this
//! sandbox only, what the proxy answers, policy checks and the activity log.
//!
//! The rules are scoped to the sandbox and go away with it.
//!
//! ```sh
//! just example shell-network
//! ```

use anyhow::Result;
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::policy::{NetworkCheck, RuleChange};
use example_common::{connect, kit, sandbox};

/// A host the example denies.
const DENIED: &str = "example.org";

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("network");
    // An owned handle: it moves into the closure, the client stays borrowed.
    let policy = client.policy();
    sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        let status = |host: &str| {
            ExecSpec::new([
                "curl",
                "-sS",
                "-m",
                "10",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                &format!("https://{host}"),
            ])
        };

        println!(
            "before: {DENIED} answers {}",
            sandbox.exec(&status(DENIED)).await?.stdout
        );
        let denied = sandbox.deny_hosts([DENIED]).await?;
        println!("denied with rule {}", denied.created[0].rule_id);
        println!(
            "after: the proxy answers {}",
            sandbox.exec(&status(DENIED)).await?.stdout
        );

        // HTTP rules narrow a host down to methods and paths.
        let http = policy
            .apply_all(&[RuleChange::deny_http(["example.net"])
                .with_methods(["POST"])
                .with_path("/admin/**")
                .for_sandbox(sandbox.name())])
            .await?;
        println!("HTTP rule {}", http[0].created[0].rule_id);

        for rule in sandbox.network_rules().await? {
            println!(
                "rule {:?} on {:?} ({})",
                rule.decision,
                rule.resources,
                rule.actions.join(", ")
            );
        }
        let check = policy
            .check(&NetworkCheck::host("api.anthropic.com"))
            .await?;
        println!(
            "api.anthropic.com allowed: {} (rule {:?})",
            check.allowed, check.rule
        );

        let log = policy.log().await?.for_sandbox(sandbox.name());
        for host in log.allowed_hosts.iter().chain(&log.blocked_hosts) {
            println!("activity: {} × {}", host.host, host.count_since);
        }

        sandbox.remove_hosts([DENIED]).await?;
        Ok(())
    })
    .await
}
