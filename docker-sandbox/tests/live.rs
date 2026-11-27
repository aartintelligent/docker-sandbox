//! Integration tests against the real `sandboxd` of the current user.
//!
//! Disabled unless the `live` feature is enabled:
//!
//! ```sh
//! cargo run -p audit -- kit-compile audit/assets/kits/audit-mixin-v3
//! DOCKER_SANDBOX_LIVE_PACKAGE="$PWD/target/audit/kits/audit-mixin-v3" \
//!     cargo test -p docker-sandbox --features live --test live -- --test-threads=1
//! ```
//!
//! - `DOCKER_SANDBOX_LIVE_PACKAGE` points to a kit package produced by
//!   `audit kit-compile` whose base kit is `shell` and whose mixin writes
//!   `/home/agent/audit-mixin.txt` at install time (the test mixin of
//!   `audit`, as above).
//!   Tests that create sandboxes are skipped without it.
//! - `DOCKER_SANDBOX_LIVE_PROBE` points to the `live-probe` binary of the
//!   workspace, built for `<arch>-unknown-linux-musl`: the tests that need a
//!   program inside the sandbox (a server, an HTTP client) upload it and run
//!   it, and are skipped without it. `just test-live` sets both variables.
//! - `DOCKER_SANDBOX_LIVE_SLOW=1` also runs the session keep-alive test (~2 minutes).
//! - `clone_mode` runs `git` on the host to make the repository it clones.
//! - `secrets_reach_requests_through_the_proxy` calls the public echo service
//!   `postman-echo.com`: it only ever receives a dummy value.
//! - With the `kit-forge` feature too, the `forged` tests create sandboxes from
//!   packages forged without the CLI: `shell` and `claude` rebuilt from their
//!   published image, `claude` with two of its arguments, and a forged mixin.
//!   They need no package. The agent of `claude` is never started: resolved
//!   as the CLI resolves it, the kit is authorized the credential the daemon
//!   holds for `anthropic`, which the proxy would write into requests to the
//!   hosts of Anthropic, and the tests send none. `proxied_keys_*` sends a
//!   dummy key to the public echo service `postman-echo.com`.
//!
//! Every resource created by these tests is removed afterwards, and the image
//! store is checked to be back to its initial state.

#![cfg(feature = "live")]
#![allow(
    clippy::unwrap_used,
    clippy::print_stderr,
    reason = "test helpers fail the test by panicking, and live tests report their progress"
)]

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use docker_sandbox::event::EventKind;
use docker_sandbox::exec::{AgentSessionSpec, AttachSpec, ExecSpec, TerminalSize};
use docker_sandbox::image::ImageSummary;
use docker_sandbox::kit::KitPackage;
use docker_sandbox::mcp::McpServerCheck;
use docker_sandbox::policy::{Decision, NetworkCheck, RuleChange};
use docker_sandbox::sandbox::{
    CreateEvent, CreateRequest, CredentialsUpdate, CustomSecret, DeleteOptions, PortBinding,
    Sandbox, SandboxState, SkillsMode, StartOptions, Workspace,
};
use docker_sandbox::{Client, Error, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Variable naming the kit package the tests create sandboxes from.
const PACKAGE_ENV: &str = "DOCKER_SANDBOX_LIVE_PACKAGE";
/// Variable enabling the slow keep-alive test.
const SLOW_ENV: &str = "DOCKER_SANDBOX_LIVE_SLOW";
/// File written by the install command of the test mixin.
const MIXIN_MARKER: &str = "/home/agent/audit-mixin.txt";
/// How long the tests wait for an exit code or an output.
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// The kit package of [`PACKAGE_ENV`], or `None` to skip the test.
fn package() -> Option<KitPackage> {
    let Some(dir) = std::env::var_os(PACKAGE_ENV).map(PathBuf::from) else {
        eprintln!("skipped: set {PACKAGE_ENV} to a kit package produced by `audit kit-compile`");
        return None;
    };
    Some(KitPackage::open(dir).expect("readable kit package"))
}

/// Variable naming the `live-probe` binary the tests run inside sandboxes.
const PROBE_ENV: &str = "DOCKER_SANDBOX_LIVE_PROBE";

/// Where `live-probe` is uploaded in a sandbox.
const PROBE: &str = "/tmp/live-probe";

/// The `live-probe` binary of [`PROBE_ENV`], or `None` to skip the test.
fn probe() -> Option<Vec<u8>> {
    let Some(path) = std::env::var_os(PROBE_ENV) else {
        eprintln!(
            "skipped: set {PROBE_ENV} to live-probe built for <arch>-unknown-linux-musl (`just test-live` does)"
        );
        return None;
    };
    Some(std::fs::read(path).expect("readable live-probe binary"))
}

/// Uploads `probe` to [`PROBE`] in `sandbox`, executable.
async fn install_probe(sandbox: &Sandbox, probe: Vec<u8>) -> Result<()> {
    sandbox.write_file_with_mode(PROBE, probe, 0o755).await
}

/// A sandbox name unique to this test run.
fn sandbox_name(test: &str) -> String {
    format!("docker-sandbox-{test}-{}", std::process::id())
}

/// References of every image held by the daemon.
async fn image_references(client: &Client) -> Result<BTreeSet<String>> {
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
async fn cleanup(client: &Client, name: &str, images_before: &BTreeSet<String>) -> Result<()> {
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

/// A tar archive holding a single file.
fn tar_with_file(name: &str, content: &[u8]) -> Bytes {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, name, content).unwrap();
    Bytes::from(builder.into_inner().unwrap())
}

/// Content of the first file of a tar archive.
fn first_file(archive: &[u8]) -> Vec<u8> {
    let mut archive = tar::Archive::new(archive);
    let mut entry = archive
        .entries()
        .unwrap()
        .next()
        .expect("archive has an entry")
        .unwrap();
    let mut content = Vec::new();
    entry.read_to_end(&mut content).unwrap();
    content
}

/// Terminal output as text, without carriage returns.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\r', "")
}

/// Daemon routes that change nothing, and error answers.
#[tokio::test]
async fn read_only_endpoints() -> Result<()> {
    let client = Client::connect().await?;
    let daemon = client.daemon();

    assert!(daemon.health().await?.is_healthy());
    assert!(daemon.info().await?.api_socket.is_some());
    assert!(
        daemon
            .settings()
            .await?
            .iter()
            .any(|s| s.key == "sandbox.disk.dockerVolume")
    );
    assert!(daemon.setting("browser.enabled").await?.value.is_boolean());
    assert!(daemon.log_levels().await?.contains_key("general"));
    assert!(!daemon.disk_usage().await?.summaries.is_empty());
    client.sandboxes().list().await?;
    assert!(!client.images().list().await?.is_empty());
    assert!(
        client
            .sandboxes()
            .find("docker-sandbox-does-not-exist")
            .await?
            .is_none()
    );
    assert!(!client.policy().rules().await?.rules.is_empty());
    client.policy().log().await?;
    client.mcp().gateway_mode().await?;
    assert!(daemon.debug_state().await?.is_object());
    assert!(daemon.diagnostics().await?.is_object());

    let policy = client.policy();
    policy.profiles().await?;
    assert!(!policy.setup_needed().await?);
    let check = policy
        .check(&NetworkCheck::host("api.anthropic.com"))
        .await?;
    assert!(check.rule.is_some());
    let mcp = client
        .mcp()
        .check_registration(&McpServerCheck::local("docker-sandbox-live", ["true"]))
        .await?;
    assert_eq!(mcp.name, "docker-sandbox-live");

    let images = client.images().list().await?;
    let any = images.first().expect("the daemon holds images").reference();
    client.images().local_metadata(&any).await?;
    assert!(client.images().exists(&any).await?);
    let mut prompts = client.prompts().subscribe().await?;
    assert!(
        tokio::time::timeout(EXIT_TIMEOUT, prompts.next())
            .await
            .expect("synced line")
            .expect("prompt stream item")?
            .is_synced()
    );
    drop(client.events().subscribe_live().await?);

    // Error answers, measured by the `errors` suite of the audit.
    assert!(
        daemon
            .setting("zz.docker-sandbox.none")
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(
        daemon
            .set_log_level("general", "zz-none")
            .await
            .unwrap_err()
            .is_invalid_input()
    );
    assert!(
        client
            .images()
            .export("zz-docker-sandbox-none:latest")
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(
        !client
            .images()
            .exists("zz-docker-sandbox-none:latest")
            .await?
    );
    assert!(
        client
            .prompts()
            .respond("zz-none", "zz-none")
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(matches!(
        policy.remove_rule("zz-docker-sandbox-none").await,
        Err(Error::Failed { .. })
    ));
    let ghost = client.sandbox("zz-docker-sandbox-none");
    assert!(ghost.start().await.unwrap_err().is_not_found());
    assert!(ghost.hold_session().await.unwrap_err().is_not_found());
    Ok(())
}

/// Settings and log levels, changed then restored.
#[tokio::test]
async fn settings_round_trip() -> Result<()> {
    let client = Client::connect().await?;
    let daemon = client.daemon();
    let key = "sandbox.disk.dockerVolume";
    let before = daemon.setting(key).await?;

    let changed = daemon.set_setting(key, "12g").await;
    let restored = if before.source.as_deref() == Some("default") {
        daemon.unset_setting(key).await
    } else {
        daemon
            .set_setting(key, before.value.clone())
            .await
            .map(drop)
    };
    assert_eq!(changed?.source.as_deref(), Some("override"));
    restored?;
    let after = daemon.setting(key).await?;
    assert_eq!((after.value, after.source), (before.value, before.source));

    let levels = daemon.log_levels().await?;
    daemon.set_log_level("general", &levels["general"]).await?;
    assert_eq!(daemon.log_levels().await?, levels);
    client.policy().refresh().await?;
    Ok(())
}

#[tokio::test]
async fn sandbox_lifecycle_from_a_kit_package() -> Result<()> {
    let Some(package) = package() else {
        return Ok(());
    };
    let client = Client::connect().await?;
    let name = sandbox_name("lifecycle");
    let images_before = image_references(&client).await?;

    let outcome = catch_unwind(exercise_lifecycle(&client, &package, &name)).await;
    cleanup(&client, &name, &images_before).await?;
    resumed(outcome)
}

/// Life cycle, files, ports, commands and network rules of a sandbox.
async fn exercise_lifecycle(client: &Client, package: &KitPackage, name: &str) -> Result<()> {
    // Load the kit sources through the raw image API rather than KitPackage.
    for image in package.images() {
        client
            .images()
            .load(Bytes::from(std::fs::read(&image.path).unwrap()))
            .await?;
    }
    client.images().pull("docker.io/sbx/shell:latest").await?;
    let created = client.sandboxes().create(&package.request(name)?).await?;
    assert_eq!(created.name, name);
    assert_eq!(created.status, SandboxState::Running);

    let sandbox = client.sandbox(name);
    assert_eq!(
        sandbox.start_with(&StartOptions::default()).await?.status,
        SandboxState::Running
    );
    sandbox
        .push_credentials(&CredentialsUpdate::default())
        .await?;

    let uname = sandbox.exec(&ExecSpec::new(["uname", "-s"])).await?;
    assert_eq!(uname.stdout.trim(), "Linux");
    assert_eq!(
        sandbox
            .exec(&ExecSpec::new(["id", "-u"]).with_user("0"))
            .await?
            .stdout
            .trim(),
        "0"
    );
    assert_eq!(
        sandbox
            .exec(&ExecSpec::new(["sh", "-c", "exit 7"]))
            .await?
            .exit_code,
        7
    );
    assert!(
        sandbox
            .exec(&ExecSpec::new(["cat", MIXIN_MARKER]))
            .await?
            .success(),
        "kit install command did not run"
    );

    sandbox
        .upload_archive(
            "/tmp",
            tar_with_file("docker-sandbox.txt", b"written by docker-sandbox\n"),
        )
        .await?;
    assert_eq!(
        first_file(&sandbox.download_archive("/tmp/docker-sandbox.txt").await?),
        b"written by docker-sandbox\n"
    );

    exercise_ports_and_paths(&sandbox).await?;
    exercise_files(&sandbox).await?;
    exercise_network_rules(&sandbox).await?;
    assert!(sandbox.vm_metrics().await?.contains("# TYPE"));
    sandbox.set_working_dir("/tmp").await?;
    assert_eq!(sandbox.mcp_gateway().await?.state.as_deref(), Some("ready"));
    assert_eq!(sandbox.stop().await?.status, SandboxState::Stopped);
    exercise_stopped(client, &sandbox, name).await
}

#[tokio::test]
async fn streams_and_interactive_sessions() -> Result<()> {
    let Some(package) = package() else {
        return Ok(());
    };
    let client = Client::connect().await?;
    let name = sandbox_name("streams");
    let images_before = image_references(&client).await?;

    // Collect the lifecycle events of our sandbox until it is deleted.
    let mut events = client.events().subscribe_live().await?;
    let watched = name.clone();
    let collector = tokio::spawn(async move {
        let mut actions = Vec::new();
        while let Some(event) = events.next().await {
            let event = event.expect("event stream item");
            if event.kind() == EventKind::SandboxLifecycle && event.is_about(&watched) {
                actions.push(event.action.clone());
                if event.action == "deleted" {
                    break;
                }
            }
        }
        actions
    });

    let outcome = catch_unwind(exercise_streams(&client, &package, &name)).await;
    cleanup(&client, &name, &images_before).await?;
    resumed(outcome)?;

    let actions = tokio::time::timeout(Duration::from_secs(30), collector)
        .await
        .expect("deleted event")
        .unwrap();
    assert_eq!(
        actions.first().map(String::as_str),
        Some("created"),
        "lifecycle events: {actions:?}"
    );
    assert_eq!(
        actions.last().map(String::as_str),
        Some("deleted"),
        "lifecycle events: {actions:?}"
    );
    Ok(())
}

/// Ports, desired ports, host paths, MCP servers and execution errors of a
/// running sandbox.
async fn exercise_ports_and_paths(sandbox: &Sandbox) -> Result<()> {
    let host_port = 40_000 + u16::try_from(std::process::id() % 20_000).unwrap();
    let published = sandbox
        .publish(&[PortBinding::tcp(8099, host_port)])
        .await?;
    assert!(
        sandbox
            .ports()
            .await?
            .iter()
            .any(|p| p.host_port == host_port)
    );
    let moved = PortBinding::tcp(8099, host_port + 1);
    sandbox.replace_desired_port(&published[0], &moved).await?;
    sandbox.unpublish(&published).await?;
    sandbox.unpublish(std::slice::from_ref(&moved)).await?;
    assert!(
        sandbox
            .ports()
            .await?
            .iter()
            .all(|p| p.host_port != host_port)
    );

    let host = tempfile::tempdir().unwrap();
    let host_path = host.path().to_str().unwrap();
    sandbox.allow_host_path(host_path).await?;
    assert!(
        sandbox
            .inspect()
            .await?
            .runtime_mounts
            .iter()
            .any(|m| m.host_path == host_path)
    );
    sandbox.revoke_host_path(host_path).await?;

    assert!(
        sandbox
            .load_mcp_server("zz-docker-sandbox-none")
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(
        sandbox
            .unload_mcp_server("zz-docker-sandbox-none")
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(
        sandbox
            .exec_inspect("zz-none")
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(
        sandbox
            .exec(&ExecSpec::new(Vec::<String>::new()))
            .await
            .unwrap_err()
            .is_invalid_input()
    );
    assert_eq!(
        sandbox.exec(&ExecSpec::new(["zz-none"])).await?.exit_code,
        127
    );

    Ok(())
}

/// A stopped sandbox: refused operations, a save, then the deletion guard and
/// a plain deletion.
async fn exercise_stopped(client: &Client, sandbox: &Sandbox, name: &str) -> Result<()> {
    assert!(
        sandbox
            .exec(&ExecSpec::new(["true"]))
            .await
            .unwrap_err()
            .is_conflict()
    );
    assert!(
        sandbox
            .read_file("/tmp/run.sh")
            .await
            .unwrap_err()
            .is_conflict()
    );

    // Saved as an image, exported, then removed.
    let tag = format!("{name}:saved");
    let saved = sandbox.save(&tag).await?;
    assert!(saved.tag.ends_with(&tag), "{}", saved.tag);
    assert!(!client.images().export(&saved.tag).await?.is_empty());
    client.images().remove(&saved.tag).await?;

    assert!(
        sandbox
            .delete_with(&DeleteOptions::default().with_expected_id("zz-none"))
            .await
            .unwrap_err()
            .is_conflict()
    );
    sandbox.delete().await?;
    assert!(!sandbox.exists().await?);
    Ok(())
}

/// File helpers, failing commands and per-command deadlines.
async fn exercise_files(sandbox: &Sandbox) -> Result<()> {
    // File helpers: single files, then directories in both directions.
    sandbox
        .write_file_with_mode("/tmp/run.sh", "#!/bin/sh\necho helper\n", 0o755)
        .await?;
    assert_eq!(
        sandbox
            .exec(&ExecSpec::new(["/tmp/run.sh"]))
            .await?
            .check()?
            .stdout,
        "helper\n"
    );
    assert_eq!(
        &sandbox.read_file("/tmp/run.sh").await?[..],
        b"#!/bin/sh\necho helper\n"
    );
    let host = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(host.path().join("tree/nested")).unwrap();
    std::fs::write(host.path().join("tree/nested/a.txt"), "a\n").unwrap();
    sandbox.upload_dir(host.path().join("tree"), "/tmp").await?;
    assert_eq!(
        sandbox
            .exec(&ExecSpec::new(["cat", "/tmp/tree/nested/a.txt"]))
            .await?
            .stdout,
        "a\n"
    );
    sandbox
        .download_dir("/tmp/tree", host.path().join("back"))
        .await?;
    assert_eq!(
        std::fs::read_to_string(host.path().join("back/tree/nested/a.txt")).unwrap(),
        "a\n"
    );

    // A failing command is an error once checked, and a command has its own
    // deadline.
    let failed = sandbox
        .exec(&ExecSpec::shell("echo nope >&2; exit 4"))
        .await?
        .check();
    assert!(matches!(failed, Err(Error::CommandFailed(ref o)) if o.exit_code == 4));
    let slow = ExecSpec::new(["sleep", "5"]).with_timeout(Duration::from_secs(1));
    assert!(sandbox.exec(&slow).await.unwrap_err().is_timeout());

    Ok(())
}

/// Network rules scoped to the sandbox, added then removed.
async fn exercise_network_rules(sandbox: &Sandbox) -> Result<()> {
    // Network rules scoped to the sandbox.
    let created = sandbox
        .allow_hosts(["docker-sandbox-live.example.com"])
        .await?;
    assert_eq!(created.created.len(), 1);
    let rules = sandbox.network_rules().await?;
    assert!(rules.iter().any(|r| {
        r.decision == Decision::Allow
            && r.resources
                .iter()
                .any(|h| h == "docker-sandbox-live.example.com")
    }));
    sandbox
        .remove_hosts(["docker-sandbox-live.example.com"])
        .await?;
    assert!(sandbox.network_rules().await?.iter().all(|r| {
        r.resources
            .iter()
            .all(|h| h != "docker-sandbox-live.example.com")
    }));

    let denied = sandbox
        .deny_hosts(["docker-sandbox-live-deny.example.com"])
        .await?;
    assert_eq!(denied.created.len(), 1);
    sandbox
        .remove_hosts(["docker-sandbox-live-deny.example.com"])
        .await?;

    let policy = sandbox.client().policy();
    let http = policy
        .apply_all(&[
            RuleChange::deny_http(["docker-sandbox-live-http.example.com"])
                .with_methods(["POST"])
                .with_path("/admin/**")
                .for_sandbox(sandbox.name()),
        ])
        .await?;
    let rule = &http[0].created[0].rule_id;
    // The global policy does not hold the rules of a sandbox.
    assert!(matches!(
        policy.remove_rule(rule).await,
        Err(Error::Failed { ref message, .. }) if message.contains("not in the global policy")
    ));
    sandbox.remove_network_rule(rule).await?;
    assert!(
        sandbox.network_rules().await?.is_empty(),
        "rules left behind"
    );
    Ok(())
}

/// Creation progress, interactive executions, agent session and SSH.
async fn exercise_streams(client: &Client, package: &KitPackage, name: &str) -> Result<()> {
    package.load_images(client).await?;
    let mut creation = client
        .sandboxes()
        .create_stream(&package.request(name)?)
        .await?;
    let mut progress = 0;
    let created = loop {
        match creation.next().await.expect("creation outcome")? {
            CreateEvent::Progress(_) => progress += 1,
            CreateEvent::Result(created) => break created,
            CreateEvent::Error(message) => panic!("creation failed: {message}"),
            _ => {}
        }
    };
    assert_eq!(created.name, name);
    assert!(progress > 0, "no progress reported");

    let sandbox = client.sandbox(name);
    let _session = sandbox.ensure_running().await?;

    // Without a terminal, stdout and stderr are kept apart.
    let spec = AttachSpec::new(["sh", "-c", "echo out; echo err >&2; exit 3"]);
    let output = sandbox
        .attach(&spec)
        .await?
        .collect("", EXIT_TIMEOUT)
        .await?;
    assert_eq!(
        (text(&output.stdout), text(&output.stderr), output.exit_code),
        ("out\n".into(), "err\n".into(), 3)
    );

    // Standard input is delivered, then closed.
    let output = sandbox
        .attach(&AttachSpec::new(["cat"]))
        .await?
        .collect("via stdin\n", EXIT_TIMEOUT)
        .await?;
    assert_eq!(text(&output.stdout), "via stdin\n");
    assert_eq!(output.exit_code, 0);

    // With a terminal, the initial size is applied…
    let spec = AttachSpec::new(["stty", "size"]).with_tty(TerminalSize::new(40, 120));
    let output = sandbox
        .attach(&spec)
        .await?
        .collect("", EXIT_TIMEOUT)
        .await?;
    assert_eq!(text(&output.stdout).trim(), "40 120");

    // …and can be changed while the process runs.
    let spec =
        AttachSpec::new(["sh", "-c", "sleep 1; stty size"]).with_tty(TerminalSize::new(24, 80));
    let (_input, mut output, control) = sandbox.attach(&spec).await?.split();
    control.resize(TerminalSize::new(50, 100)).await?;
    assert_eq!(control.inspect().await?.id, control.id());
    let mut seen = Vec::new();
    while let Some(chunk) = output.next().await {
        seen.extend_from_slice(&chunk?.data);
    }
    assert_eq!(text(&seen).trim(), "50 100");
    assert_eq!(control.wait(EXIT_TIMEOUT).await?, 0);

    // The agent of the shell kit is an interactive bash.
    let (mut input, mut output, control) = sandbox
        .agent_session(&AgentSessionSpec::default())
        .await?
        .split();
    input
        .write_all(b"echo agent-$((40 + 2))\nexit\n")
        .await
        .unwrap();
    let mut transcript = Vec::new();
    while let Some(chunk) = tokio::time::timeout(EXIT_TIMEOUT, output.next())
        .await
        .expect("agent output")
        .transpose()?
    {
        transcript.extend_from_slice(&chunk.data);
    }
    assert!(
        text(&transcript).contains("agent-42"),
        "agent transcript: {}",
        text(&transcript)
    );
    assert_eq!(control.wait(EXIT_TIMEOUT).await?, 0);

    // The SSH server greets through the tunnel.
    let mut tunnel = sandbox.ssh_tunnel().await?;
    let mut banner = vec![0_u8; 8];
    tokio::time::timeout(EXIT_TIMEOUT, tunnel.read_exact(&mut banner))
        .await
        .expect("SSH banner")
        .unwrap();
    assert_eq!(&banner, b"SSH-2.0-");
    Ok(())
}

#[tokio::test]
async fn kit_added_to_a_running_sandbox() -> Result<()> {
    let Some(package) = package() else {
        return Ok(());
    };
    let client = Client::connect().await?;
    let name = sandbox_name("hotadd");
    let images_before = image_references(&client).await?;

    let outcome = catch_unwind(exercise_hot_add(&client, &package, &name)).await;
    cleanup(&client, &name, &images_before).await?;
    resumed(outcome)
}

/// A mixin added to a running sandbox, keeping its changes.
async fn exercise_hot_add(client: &Client, package: &KitPackage, name: &str) -> Result<()> {
    // A sandbox made of the base kit only: the first kit of the package.
    let mut base_only: CreateRequest = package.request(name)?;
    base_only.kits.truncate(1);
    base_only.kit_artifacts.truncate(1);
    client.sandboxes().create(&base_only).await?;

    let sandbox = client.sandbox(name);
    let _session = sandbox.ensure_running().await?;
    sandbox
        .upload_archive("/home/agent", tar_with_file("kept.txt", b"kept\n"))
        .await?;
    assert!(
        !sandbox
            .exec(&ExecSpec::new(["test", "-e", MIXIN_MARKER]))
            .await?
            .success(),
        "mixin present too early"
    );

    let id_before = sandbox.inspect().await?.id;
    package.add_to(&sandbox).await?;
    sandbox.start().await?;

    assert_eq!(
        sandbox.inspect().await?.id,
        id_before,
        "sandbox identity changed"
    );
    assert!(
        sandbox
            .exec(&ExecSpec::new(["cat", MIXIN_MARKER]))
            .await?
            .success(),
        "mixin not installed"
    );
    assert_eq!(
        sandbox
            .exec(&ExecSpec::new(["cat", "/home/agent/kept.txt"]))
            .await?
            .stdout,
        "kept\n",
        "changes lost"
    );
    Ok(())
}

#[tokio::test]
async fn session_keeps_the_sandbox_running() -> Result<()> {
    if std::env::var_os(SLOW_ENV).is_none() {
        eprintln!("skipped: set {SLOW_ENV}=1 to run this ~2 minute test");
        return Ok(());
    }
    let Some(package) = package() else {
        return Ok(());
    };
    let client = Client::connect().await?;
    let name = sandbox_name("keepalive");
    let images_before = image_references(&client).await?;

    let outcome = catch_unwind(exercise_keep_alive(&client, &package, &name)).await;
    cleanup(&client, &name, &images_before).await?;
    let stopped_after = resumed(outcome)?;
    eprintln!("sandbox stopped {stopped_after:?} after its session was released");
    Ok(())
}

/// A mixin installed into a running sandbox without recreating it.
#[tokio::test]
async fn kit_installed_in_place() -> Result<()> {
    let Some(package) = package() else {
        return Ok(());
    };
    let client = Client::connect().await?;
    let name = sandbox_name("inplace");
    let images_before = image_references(&client).await?;

    let outcome = catch_unwind(exercise_in_place(&client, &package, &name)).await;
    cleanup(&client, &name, &images_before).await?;
    resumed(outcome)
}

/// Installs the mixin of the package into a base-only sandbox.
async fn exercise_in_place(client: &Client, package: &KitPackage, name: &str) -> Result<()> {
    let mut base_only: CreateRequest = package.request(name)?;
    base_only.kits.truncate(1);
    base_only.kit_artifacts.truncate(1);
    client.sandboxes().create(&base_only).await?;

    let sandbox = client.sandbox(name);
    let _session = sandbox.ensure_running().await?;
    let id_before = sandbox.inspect().await?.id;
    assert_eq!(package.install_into(&sandbox).await?, 1);
    assert_eq!(
        sandbox
            .exec(&ExecSpec::new(["cat", MIXIN_MARKER]))
            .await?
            .check()?
            .stdout,
        "audit-mixin-v3 installed\n"
    );
    assert_eq!(sandbox.inspect().await?.id, id_before, "sandbox recreated");
    Ok(())
}

/// How long a sandbox runs once its session is released.
async fn exercise_keep_alive(
    client: &Client,
    package: &KitPackage,
    name: &str,
) -> Result<Duration> {
    package.create(client, name).await?;
    let sandbox = client.sandbox(name);
    let session = sandbox.ensure_running().await?;

    tokio::time::sleep(Duration::from_secs(45)).await;
    assert!(session.is_connected());
    assert_eq!(
        sandbox.inspect().await?.status,
        SandboxState::Running,
        "stopped while the session was held"
    );

    session.release();
    let released = Instant::now();
    while released.elapsed() < Duration::from_secs(90) {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if sandbox.inspect().await?.status == SandboxState::Stopped {
            return Ok(released.elapsed());
        }
    }
    panic!("sandbox still running 90 s after its session was released");
}

/// What `sbx run` gives an agent beyond its kit: workspaces, a port and
/// skills chosen at creation, and the arguments, environment and working
/// directory of an agent session.
#[tokio::test]
async fn agent_options_of_sbx_run() -> Result<()> {
    let (Some(package), Some(probe)) = (package(), probe()) else {
        return Ok(());
    };
    let client = Client::connect().await?;
    let name = sandbox_name("agent-options");
    let images_before = image_references(&client).await?;
    let host = tempfile::tempdir().unwrap();

    let outcome = catch_unwind(exercise_agent_options(
        &client,
        &package,
        probe,
        &name,
        host.path(),
    ))
    .await;
    cleanup(&client, &name, &images_before).await?;
    resumed(outcome)
}

/// Creates a sandbox with every workspace form, a port and no skills, then
/// checks the mounts from inside and runs the agent with arguments.
async fn exercise_agent_options(
    client: &Client,
    package: &KitPackage,
    probe: Vec<u8>,
    name: &str,
    host: &Path,
) -> Result<()> {
    let (project, docs, data) = (host.join("project"), host.join("docs"), host.join("data"));
    for dir in [&project, &docs, &data] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(project.join("notes.txt"), "from the host\n").unwrap();
    std::fs::write(project.join("locked.txt"), "read-only\n").unwrap();
    std::fs::write(docs.join("readme.txt"), "docs\n").unwrap();
    let path = |p: &Path| p.display().to_string();
    let host_port = free_port();

    let request = package
        .request(name)?
        .with_workspace(path(&project))
        .with_additional_workspace(Workspace::new(path(&docs)).read_only())
        .with_additional_workspace(Workspace::new(path(&data)).at("/workspace/data"))
        .with_additional_workspace(Workspace::new(path(&project.join("locked.txt"))).read_only())
        .with_port(PortBinding::tcp(8085, host_port))
        .with_skills(SkillsMode::Off);
    package.create_with(client, &request).await?;
    let sandbox = client.sandbox(name);
    let _session = sandbox.ensure_running().await?;

    // A port given as `tcp` at creation is published, as one the CLI gives
    // as `tcp4`.
    let ports = sandbox.ports().await?;
    assert!(
        ports
            .iter()
            .any(|p| p.sandbox_port == 8085 && p.host_port == host_port),
        "ports: {ports:?}"
    );
    // A connection to the host port reaches a server listening in the
    // sandbox: `live-probe` answers one connection with a line, once it says
    // it listens.
    install_probe(&sandbox, probe).await?;
    let server = AttachSpec::new([PROBE, "serve-once", "8085", "hello-from-the-sandbox"]);
    let (_input, mut output, control) = sandbox.attach(&server).await?.split();
    let mut started = Vec::new();
    while !text(&started).contains("listening on") {
        let chunk = tokio::time::timeout(EXIT_TIMEOUT, output.next())
            .await
            .expect("server output")
            .unwrap_or_else(|| panic!("server ended: {}", text(&started)))?;
        started.extend_from_slice(&chunk.data);
    }
    assert_eq!(read_from_port(host_port).await, "hello-from-the-sandbox\n");
    assert_eq!(control.wait(EXIT_TIMEOUT).await?, 0);

    // The primary workspace is the working directory, writable, at its host
    // path; the others are where and how they were asked for.
    let script = format!(
        "pwd; echo from the sandbox >> notes.txt; \
         echo x > {docs}/written 2>/dev/null || echo docs-read-only; \
         echo x >> locked.txt 2>/dev/null || echo file-read-only; \
         echo from the sandbox > /workspace/data/out.txt && echo data-writable",
        docs = path(&docs)
    );
    let output = sandbox.exec(&ExecSpec::shell(script)).await?.check()?;
    assert_eq!(
        output.stdout.lines().collect::<Vec<_>>(),
        [
            path(&project).as_str(),
            "docs-read-only",
            "file-read-only",
            "data-writable"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(project.join("notes.txt")).unwrap(),
        "from the host\nfrom the sandbox\n",
        "the agent changes the files of the host"
    );
    assert_eq!(
        std::fs::read_to_string(data.join("out.txt")).unwrap(),
        "from the sandbox\n"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("locked.txt")).unwrap(),
        "read-only\n"
    );

    // Arguments starting with a flag follow the default command of the kit
    // (`bash -l`); the environment and working directory are those of the
    // session.
    let spec = AgentSessionSpec::default()
        .with_args(["-c", "echo agent-$((40 + 2)) $LIVE_ENV; pwd"])
        .with_env("LIVE_ENV", "from-the-session")
        .with_working_dir("/tmp");
    let transcript = agent_transcript(&sandbox, &spec).await?;
    assert!(
        transcript.contains("agent-42 from-the-session") && transcript.contains("/tmp"),
        "agent transcript: {transcript}"
    );
    // Arguments starting with a word replace the default command.
    let spec = AgentSessionSpec::default().with_args(["echo", "in-place-of-bash"]);
    let transcript = agent_transcript(&sandbox, &spec).await?;
    assert!(
        transcript.contains("in-place-of-bash"),
        "agent transcript: {transcript}"
    );
    Ok(())
}

/// A sandbox on a private clone of a host Git repository.
#[tokio::test]
async fn clone_mode() -> Result<()> {
    let Some(package) = package() else {
        return Ok(());
    };
    let client = Client::connect().await?;
    let name = sandbox_name("clone");
    let images_before = image_references(&client).await?;
    let host = tempfile::tempdir().unwrap();

    let outcome = catch_unwind(exercise_clone(&client, &package, &name, host.path())).await;
    cleanup(&client, &name, &images_before).await?;
    resumed(outcome)
}

/// Clones a one-commit repository into a sandbox and commits there, leaving
/// the host repository untouched.
async fn exercise_clone(
    client: &Client,
    package: &KitPackage,
    name: &str,
    host: &Path,
) -> Result<()> {
    let repository = host.join("repo");
    git(host, &["init", "-q", "-b", "main", "repo"]);
    std::fs::write(repository.join("README.md"), "clone mode\n").unwrap();
    git(&repository, &["add", "README.md"]);
    git(&repository, &["commit", "-q", "-m", "seed"]);
    let repository = repository.display().to_string();

    let request = package.request(name)?.with_cloned_workspace(&repository);
    package.create_with(client, &request).await?;
    let sandbox = client.sandbox(name);
    let _session = sandbox.ensure_running().await?;

    let script = "pwd; git log --format=%s; \
                  git -c user.name=agent -c user.email=agent@example.invalid \
                      -c commit.gpgsign=false commit -q --allow-empty -m 'agent work'; \
                  git log --format=%s -1; \
                  touch /run/sandbox/source/written 2>/dev/null || echo source-read-only";
    let output = sandbox.exec(&ExecSpec::shell(script)).await?.check()?;
    assert_eq!(
        output.stdout.lines().collect::<Vec<_>>(),
        [
            repository.as_str(),
            "seed",
            "agent work",
            "source-read-only"
        ]
    );
    let host_log = git(Path::new(&repository), &["log", "--format=%s"]);
    assert_eq!(
        host_log, "seed\n",
        "the commit of the agent stays in the sandbox"
    );
    Ok(())
}

/// Runs `git args` in `dir`, without the configuration of the user, and
/// returns its output.
///
/// # Panics
///
/// When git fails.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=live",
            "-c",
            "user.email=live@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// A TCP port free on the loopback interface of the host.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Everything an agent session prints until it ends; its exit code must be 0.
async fn agent_transcript(sandbox: &Sandbox, spec: &AgentSessionSpec) -> Result<String> {
    // The input stays open until the output ends: closing it at once can end
    // the session before its output reaches the client.
    let (_input, mut output, control) = sandbox.agent_session(spec).await?.split();
    let mut transcript = Vec::new();
    while let Some(chunk) = tokio::time::timeout(EXIT_TIMEOUT, output.next())
        .await
        .expect("agent output")
        .transpose()?
    {
        transcript.extend_from_slice(&chunk.data);
    }
    assert_eq!(control.wait(EXIT_TIMEOUT).await?, 0);
    Ok(text(&transcript))
}

/// What the server behind `127.0.0.1:port` of the host sends, retrying while
/// the published port does not answer yet.
///
/// # Panics
///
/// When nothing answers within [`EXIT_TIMEOUT`].
async fn read_from_port(port: u16) -> String {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    loop {
        if let Ok(mut stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            let mut reply = String::new();
            stream.read_to_string(&mut reply).await.unwrap();
            if !reply.is_empty() {
                return reply;
            }
        }
        assert!(
            Instant::now() < deadline,
            "nothing answered on port {port} of the host"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Dummy value of the custom secret of [`secrets_reach_requests_through_the_proxy`].
const DUMMY_SECRET: &str = "live-dummy-value-not-a-secret";

/// Public service answering with the headers it received, over HTTPS.
const ECHO_HOST: &str = "postman-echo.com";

/// Host of a second secret, which the requests of the test never reach.
const OTHER_HOST: &str = "api.example.com";

/// Dummy value of the secret for [`OTHER_HOST`].
const OTHER_SECRET: &str = "live-other-dummy-value";

/// File the search of the values must find, to show that it reads files.
const SEARCH_CANARY: &str = "/tmp/live-search-canary";

/// A custom secret pushed to a sandbox, as the proxy applies it to a request
/// to a public echo service: what the service receives in place of the
/// placeholder, and of the placeholder of a secret for another host, through
/// a restart, and once the secrets are removed. Only dummy values leave the
/// machine.
#[tokio::test]
async fn secrets_reach_requests_through_the_proxy() -> Result<()> {
    let (Some(package), Some(probe)) = (package(), probe()) else {
        return Ok(());
    };
    let client = Client::connect().await?;
    let name = sandbox_name("secrets");
    let images_before = image_references(&client).await?;

    let outcome = catch_unwind(exercise_secrets(&client, &package, probe, &name)).await;
    cleanup(&client, &name, &images_before).await?;
    resumed(outcome)
}

/// Pushes a custom secret for the echo service, then asks it what it receives
/// for the placeholder, before and after a restart and after the removal.
async fn exercise_secrets(
    client: &Client,
    package: &KitPackage,
    probe: Vec<u8>,
    name: &str,
) -> Result<()> {
    let sentinel = format!("sbx-cs-live{}", std::process::id());
    package.create_with(client, &package.request(name)?).await?;
    let sandbox = client.sandbox(name);
    let session = sandbox.ensure_running().await?;
    install_probe(&sandbox, probe).await?;

    let other = format!("sbx-cs-other{}", std::process::id());
    let secret = CustomSecret::new([ECHO_HOST], &sentinel, DUMMY_SECRET).with_env_name("LIVE_KEY");
    let elsewhere = CustomSecret::new([OTHER_HOST], &other, OTHER_SECRET);
    sandbox
        .push_credentials(
            &CredentialsUpdate::default()
                .with_custom_secret(secret)
                .with_custom_secret(elsewhere),
        )
        .await?;
    // `live-probe` sends a placeholder; the service answers with the headers
    // it received, read here as a verdict, never printed.
    let verdict_for = |sandbox: &Sandbox, sentinel: &str| {
        let probe = ExecSpec::new([
            PROBE.to_owned(),
            "http".to_owned(),
            format!("http://{ECHO_HOST}/headers"),
            "Authorization".to_owned(),
            format!("Bearer {sentinel}"),
        ]);
        let (sandbox, sentinel) = (sandbox.clone(), sentinel.to_owned());
        async move {
            let output = sandbox.exec(&probe).await?.check()?;
            let echo: serde_json::Value =
                serde_json::from_str(&output.stdout).expect("JSON from the echo service");
            let header = echo["headers"]["authorization"].as_str();
            Ok::<_, Error>(match header {
                None => "absent",
                Some(h) if h == format!("Bearer {DUMMY_SECRET}") => "dummy value",
                Some(h) if h == format!("Bearer {sentinel}") => "placeholder",
                Some(_) => "other",
            })
        }
    };
    let verdict = |sandbox: &Sandbox| verdict_for(sandbox, &sentinel);

    // The proxy puts the value in place of the placeholder; the sandbox
    // never holds it.
    assert_eq!(verdict(&sandbox).await?, "dummy value");
    // A secret goes to its own hosts only: the placeholder of the other one
    // reaches the echo service as it is.
    assert_eq!(
        verdict_for(&sandbox, &other).await?,
        "placeholder",
        "a secret for another host"
    );
    // No file of the sandbox holds a value: a search run as root, so that no
    // file escapes it, finds the file written for it and nothing else.
    let canary = format!("live-search-canary-{}", std::process::id());
    sandbox
        .write_file(SEARCH_CANARY, format!("{canary}\n"))
        .await?;
    let search = ExecSpec::new([
        "grep",
        "-rlsF",
        "--exclude-dir=proc",
        "--exclude-dir=sys",
        "--exclude-dir=dev",
        "-e",
        DUMMY_SECRET,
        "-e",
        OTHER_SECRET,
        "-e",
        &canary,
        "/",
    ])
    .with_user("root")
    .with_timeout(Duration::from_secs(120));
    let found = sandbox.exec(&search).await?.stdout;
    assert_eq!(found.lines().collect::<Vec<_>>(), [SEARCH_CANARY]);
    // The daemon keeps the secret through a restart: nothing to push again.
    drop(session);
    sandbox.stop().await?;
    let session = sandbox.ensure_running().await?;
    assert_eq!(verdict(&sandbox).await?, "dummy value", "after a restart");
    // An empty update, as `ensure_running` sends, leaves the secrets alone.
    sandbox
        .push_credentials(&CredentialsUpdate::default())
        .await?;
    assert_eq!(
        verdict(&sandbox).await?,
        "dummy value",
        "after an empty push"
    );
    // An empty list removes them: the placeholder leaves as it is.
    sandbox
        .push_credentials(&CredentialsUpdate::default().without_custom_secrets())
        .await?;
    assert_eq!(verdict(&sandbox).await?, "placeholder", "after the removal");
    drop(session);
    Ok(())
}

/// Sandboxes created from packages forged without the CLI (feature
/// `kit-forge`): the base kit rebuilt from its published image, and a mixin
/// forged from its descriptor.
#[cfg(feature = "kit-forge")]
mod forged {
    use docker_sandbox::kit::{KitDescriptor, KitForge, LifecycleStep, Mixin};
    use docker_sandbox::sandbox::ProxiedKey;

    use super::*;

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
            .with_install(
                LifecycleStep::new(format!("echo installed > {INSTALLED}")).as_user("agent"),
            )
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
        let started = format!(
            "for i in $(seq 20); do [ -e {STARTED} ] && break; sleep 0.5; done; cat {STARTED}"
        );
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
            assert!(created.withheld.is_empty(), "withheld {:?}", created.withheld);

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
                    .exec(&ExecSpec::new(["test", "-s", "/home/agent/.claude/CLAUDE.md"]))
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
            let outcome = sandbox.push_proxied_keys(std::slice::from_ref(&key)).await?;
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
            assert!(echoed.contains(DUMMY_KEY), "the key did not reach its host: {echoed}");
            assert!(!echoed.contains(key.placeholder()), "{echoed}");
            Ok(())
        })
        .await
    }
}
