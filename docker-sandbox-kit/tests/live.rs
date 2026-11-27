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
//! its arguments, and a forged mixin. They need no package and no `sbx` CLI.
//! The agent of `claude` is never started: resolved as the CLI resolves it,
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

use std::collections::BTreeSet;
use std::path::PathBuf;

use docker_sandbox::Client;
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::image::ImageSummary;
use docker_sandbox::sandbox::{ProxiedKey, Sandbox};
use docker_sandbox_kit::{KitDescriptor, KitForge, LifecycleStep, Mixin, Result};

/// A sandbox name unique to this test run.
fn sandbox_name(test: &str) -> String {
    format!("docker-sandbox-{test}-{}", std::process::id())
}

/// References of every image held by the daemon.
async fn image_references(client: &Client) -> docker_sandbox::Result<BTreeSet<String>> {
    Ok(client
        .images()
        .list()
        .await?
        .iter()
        .map(ImageSummary::reference)
        .collect())
}

/// Deletes the sandbox if it still exists and removes every image created since
/// `images_before`, then checks that the image store is restored.
async fn cleanup(
    client: &Client,
    name: &str,
    images_before: &BTreeSet<String>,
) -> docker_sandbox::Result<()> {
    let sandbox = client.sandbox(name);
    if sandbox.exists().await? {
        sandbox.force_delete().await?;
    }
    // Removing the last tag of an image can reveal it under its digest
    // reference: list again until nothing new is left.
    for _ in 0..3 {
        let mut leftovers: Vec<String> = image_references(client)
            .await?
            .difference(images_before)
            .cloned()
            .collect();
        if leftovers.is_empty() {
            break;
        }
        // Derived tags go before the image they point to.
        leftovers.sort_by_key(|r| std::cmp::Reverse(r.len()));
        for reference in &leftovers {
            client.images().remove(reference).await?;
        }
    }
    assert!(
        client.sandboxes().find(name).await?.is_none(),
        "sandbox {name} was not deleted"
    );
    assert_eq!(
        &image_references(client).await?,
        images_before,
        "image store not restored"
    );
    Ok(())
}

/// Runs `exercise`, turning a panic — a failed assertion — into the `Err` of
/// its payload, so that the test still cleans up before failing.
async fn catch_unwind<F: Future>(exercise: F) -> std::thread::Result<F::Output> {
    let mut exercise = std::pin::pin!(exercise);
    std::future::poll_fn(|cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| exercise.as_mut().poll(cx)))
        {
            Ok(std::task::Poll::Ready(output)) => std::task::Poll::Ready(Ok(output)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(panic) => std::task::Poll::Ready(Err(panic)),
        }
    })
    .await
}

/// The output of an exercise run by [`catch_unwind`], or its panic again, once
/// the test has cleaned up.
fn resumed<T>(outcome: std::thread::Result<T>) -> T {
    outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

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

/// The standard output of `script`, run with `sh -c` in `sandbox`.
async fn shell(sandbox: &Sandbox, script: &str) -> Result<String> {
    Ok(sandbox
        .exec(&ExecSpec::new(["sh", "-c", script]))
        .await?
        .check()?
        .stdout)
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

/// Runs `exercise` on the sandbox `test`, in a directory for its package,
/// then removes what it created.
async fn run<F, Fut>(test: &str, exercise: F) -> Result<()>
where
    F: FnOnce(Client, String, PathBuf) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let client = Client::connect().await?;
    let name = sandbox_name(test);
    let images_before = image_references(&client).await?;
    let dir = tempfile::tempdir().unwrap();
    let outcome = catch_unwind(exercise(
        client.clone(),
        name.clone(),
        dir.path().to_owned(),
    ))
    .await;
    cleanup(&client, &name, &images_before).await?;
    resumed(outcome)
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

/// The claude kit rebuilt from its image with two of its arguments, and
/// a forged mixin. The agent is never started.
#[tokio::test]
async fn claude_rebuilt_with_its_arguments() -> Result<()> {
    run("forged-claude", |client, name, dir| async move {
        let package = KitForge::new(client.clone())
            .with_base("docker.io/sbx/claude:latest")
            .with_arg("model", "claude-haiku-5-5")
            .with_arg("reasoningEffort", "low")
            .with_mixin(mixin())
            .forge(dir.join("claude+forged"))
            .await?;
        let created = package.create(&client, &name).await?;
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
