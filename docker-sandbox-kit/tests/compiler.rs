//! The kit compiler against a fake daemon and a fake `sbx` CLI.
//!
//! The CLI is this test binary, re-executed as the helper process
//! [`fake_sbx`]: it talks to the capturing proxy of the compiler as the real
//! CLI would, so that the whole compilation runs without Docker Sandboxes.

#![cfg(feature = "compiler")]
#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

// The fake daemon of the tests of `docker-sandbox`, shared rather than copied.
#[path = "../../docker-sandbox/tests/support/mod.rs"]
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use docker_sandbox::Client;
use docker_sandbox::sandbox::CreateRequest;
use docker_sandbox_kit::Error;
use docker_sandbox_kit::{CompileJob, KitCompiler};
use serde_json::{Value, json};
use support::{FakeDaemon, Reply, connect, sandbox_json};
use tokio::sync::Mutex;

/// Name of the helper test playing the `sbx` CLI.
const FAKE_SBX: &str = "fake_sbx";

/// Held by each test for its whole run. The tests write executables and start
/// them: running them one at a time keeps a test from starting a program
/// while another still holds one open for writing (`ETXTBSY`).
static SERIAL: Mutex<()> = Mutex::const_new(());

/// An `sbx` executable in `dir` running [`fake_sbx`].
///
/// The compiler starts the CLI with arguments of its own: a two-line `sh`
/// script passes them after `--`, where the test harness takes them as extra
/// name filters, which select no other test.
fn fake_sbx_in(dir: &Path) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = dir.join("sbx");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nexport {}={FAKE_SBX}\nexec '{}' --exact {FAKE_SBX} --nocapture -- \"$@\"\n",
            support::HELPER_ENV,
            exe.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// An OCI image layout archive whose index names `sbx-kit-src:spec-abc`.
fn oci_layout() -> Vec<u8> {
    let index = json!({ "manifests": [{ "digest": "sha256:1", "annotations": { "org.opencontainers.image.ref.name": "sbx-kit-src:spec-abc" } }] });
    support::tar_with_file("index.json", index.to_string().as_bytes())
}

/// A daemon serving what a compilation needs. When `created`, the throw-away
/// sandbox exists afterwards and cannot be deleted.
fn daemon(created: bool) -> FakeDaemon {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/docker/images/load",
            Reply::json(200, &json!({ "message": "image loaded" })),
        )
        .route(
            "POST",
            "/sandbox",
            Reply::ndjson(&[json!({ "result": { "id": "1", "name": "sbx-kit-compile", "agent": "shell", "status": "running" } })]),
        );
    if created {
        daemon
            .route(
                "GET",
                "/sandbox/*",
                Reply::json(200, &sandbox_json("sbx-kit-compile", "running")),
            )
            .start()
    } else {
        daemon.start()
    }
}

/// Plays the `sbx` CLI when started by [`fake_sbx_in`]; does nothing in a
/// normal run.
///
/// The base kit picks the behavior: `fail` exits with an error, `slow`
/// outlives the compilation timeout, `silent` exits without creating
/// anything. Any other base loads a kit image and creates the sandbox through
/// the socket the compiler points the CLI at, the CLI arguments recorded in
/// the creation request.
#[tokio::test]
async fn fake_sbx() {
    if !support::is_helper(FAKE_SBX) {
        return;
    }
    let args: Vec<String> = std::env::args()
        .skip_while(|arg| arg != "--")
        .skip(1)
        .collect();
    match args.last().unwrap().as_str() {
        "fail" => {
            for line in 1..=6 {
                eprintln!("line {line}");
            }
            std::process::exit(1);
        }
        "slow" => tokio::time::sleep(Duration::from_secs(30)).await,
        "silent" => {}
        base => {
            let name = args.iter().skip_while(|arg| *arg != "--name").nth(1);
            let client = Client::connect().await.unwrap();
            client.images().load(oci_layout()).await.unwrap();
            let template = json!({ "name": "{{name}}", "agent": base, "cli_args": args,
                                   "ssh_agent_socket_path": "/run/ssh-agent.sock",
                                   "bindings_path": "/home/agent/.config/sbx/credentials.yaml" });
            let request = CreateRequest::from_template(template, name.unwrap()).unwrap();
            client.sandboxes().create(&request).await.unwrap();
        }
    }
}

#[tokio::test]
async fn compositions_compile_into_packages() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let daemon = daemon(true);
    let client = connect(&daemon).await;
    let compiler = KitCompiler::new(fake_sbx_in(dir.path()), client, dir.path().join("work"))
        .with_timeout(Duration::from_secs(60));
    let job = CompileJob::new("shell", dir.path().join("package"))
        .with_mixin("/kits/mixin")
        .with_arg("region", "eu");

    let package = compiler.compile(&job).await.unwrap();
    assert_eq!(package.compiled_with(), Some("v0.54.0"));
    assert_eq!(
        package.images()[0].reference.as_deref(),
        Some("sbx-kit-src:spec-abc")
    );
    let request = package.request("job-42").unwrap();
    assert_eq!(request.name, "job-42");
    assert!(
        request.ssh_agent_socket_path.is_none() && request.bindings_path.is_none(),
        "the paths of the compiling machine are dropped"
    );
    let args: Vec<String> = serde_json::from_value(request.extra["cli_args"].clone()).unwrap();
    assert_eq!(args[..3], ["--no-input", "create", "--name"]);
    assert!(args[3].starts_with("sbx-kit-compile-"), "{args:?}");
    assert_eq!(
        args[4..],
        [
            "--no-progress",
            "--kit",
            "/kits/mixin",
            "--kit-arg",
            "region=eu",
            "shell"
        ]
    );

    // The throw-away sandbox is deleted, or left with a warning when the
    // daemon refuses.
    let deletions: Vec<Value> = daemon
        .requests()
        .iter()
        .filter(|r| r.method == "DELETE")
        .map(|r| json!(r.path()))
        .collect();
    assert_eq!(deletions, [json!(format!("/sandbox/{}", args[3]))]);
}

#[tokio::test]
async fn cli_failures_quote_the_end_of_its_errors() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let daemon = daemon(false);
    let compiler = KitCompiler::new(
        fake_sbx_in(dir.path()),
        connect(&daemon).await,
        dir.path().join("work"),
    );

    let error = compiler
        .compile(&CompileJob::new("fail", dir.path().join("package")))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Compile(ref m) if m == "sbx could not build the kit composition: line 2\nline 3\nline 4\nline 5\nline 6"),
        "{error}"
    );
}

#[tokio::test]
async fn a_cli_that_creates_nothing_compiles_nothing() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let daemon = daemon(false);
    let compiler = KitCompiler::new(
        fake_sbx_in(dir.path()),
        connect(&daemon).await,
        dir.path().join("work"),
    );

    let error = compiler
        .compile(&CompileJob::new("silent", dir.path().join("package")))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Compile(ref m) if m == "sbx did not send a creation request"),
        "{error}"
    );
    assert!(!dir.path().join("package").exists());
}

#[tokio::test]
async fn compilations_time_out() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let daemon = daemon(false);
    let compiler = KitCompiler::new(
        fake_sbx_in(dir.path()),
        connect(&daemon).await,
        dir.path().join("work"),
    )
    .with_timeout(Duration::from_millis(500));

    let error = compiler
        .compile(&CompileJob::new("slow", dir.path().join("package")))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Client(docker_sandbox::Error::Timeout { operation: "kit compilation", after }) if after == Duration::from_millis(500)),
        "{error}"
    );
}

#[tokio::test]
async fn a_missing_cli_or_proxy_socket_is_an_error() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let daemon = daemon(false);
    let client = connect(&daemon).await;
    let job = CompileJob::new("shell", dir.path().join("package"));

    let missing = KitCompiler::new(dir.path().join("absent"), client.clone(), dir.path());
    let error = missing.compile(&job).await.unwrap_err();
    assert!(
        matches!(error, Error::Compile(ref m) if m.starts_with("cannot run ")),
        "{error}"
    );

    // Unix socket paths are limited to about 100 bytes.
    let deep = dir.path().join("w".repeat(120));
    let compiler = KitCompiler::new(fake_sbx_in(dir.path()), client, deep);
    let error = compiler.compile(&job).await.unwrap_err();
    assert!(
        matches!(error, Error::Client(docker_sandbox::Error::Package { .. })),
        "{error}"
    );
}
