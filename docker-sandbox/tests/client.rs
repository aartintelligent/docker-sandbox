//! Connection, daemon operations and error mapping against a fake daemon, and
//! the guarantees runners rely on: owned, shareable handles and `Send`
//! futures.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

mod support;

use std::time::Duration;

use docker_sandbox::daemon::Daemon;
use docker_sandbox::event::Events;
use docker_sandbox::exec::{ExecControl, ExecSpec};
use docker_sandbox::image::Images;
use docker_sandbox::mcp::Mcp;
use docker_sandbox::policy::Policy;
use docker_sandbox::prompt::Prompts;
use docker_sandbox::sandbox::{Sandbox, Sandboxes, SessionGuard};
use docker_sandbox::{Client, Error, SOCKET_ENV, VersionPolicy};
use serde_json::json;
use support::{FakeDaemon, Reply, connect};

#[tokio::test]
async fn connect_reports_daemon_health() {
    let daemon = FakeDaemon::builder().start();
    let client = connect(&daemon).await;

    assert_eq!(client.health().api_version, "0.52.0");
    assert!(client.health().is_healthy());
    assert_eq!(client.socket_path(), daemon.socket());
}

#[tokio::test]
async fn connect_finds_the_daemon_as_the_cli_does() {
    let daemon = FakeDaemon::builder().start();
    let mut helper = support::helper_process("connect_through_the_environment");
    helper.env(SOCKET_ENV, daemon.socket());

    let status = tokio::task::spawn_blocking(move || helper.status())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "the helper process failed: {status}");
}

/// Connects with the defaults, run by `connect_finds_the_daemon_as_the_cli_does`
/// with `DOCKER_SANDBOXES_API` set; does nothing otherwise.
#[tokio::test]
async fn connect_through_the_environment() {
    if !support::is_helper("connect_through_the_environment") {
        return;
    }
    let client = Client::connect().await.unwrap();
    assert_eq!(
        client.socket_path().as_os_str(),
        std::env::var_os(SOCKET_ENV).unwrap()
    );
}

#[tokio::test]
async fn connect_enforces_the_version_policy() {
    let daemon = FakeDaemon::builder().health("0.60.0").start();

    let strict = Client::builder()
        .with_socket(daemon.socket())
        .connect()
        .await;
    assert!(
        matches!(strict, Err(Error::UnsupportedVersion { ref found, .. }) if found == "0.60.0")
    );

    let lenient = Client::builder()
        .with_socket(daemon.socket())
        .with_version_policy(VersionPolicy::Warn)
        .connect()
        .await;
    assert!(lenient.is_ok());
}

#[tokio::test]
async fn connect_fails_cleanly_without_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.sock");

    let result = Client::builder().with_socket(&missing).connect().await;
    let error = result.unwrap_err();
    assert!(matches!(error, Error::Connect { ref path, .. } if *path == missing));
    assert!(error.is_transient());
}

#[tokio::test]
async fn slow_answers_time_out_with_the_operation_name() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/daemon/info",
            Reply::json(200, &json!({})).delayed(Duration::from_millis(500)),
        )
        .start();
    let client = Client::builder()
        .with_socket(daemon.socket())
        .with_timeout(Duration::from_millis(100))
        .connect()
        .await
        .unwrap();

    let error = client.daemon().info().await.unwrap_err();
    assert!(error.is_timeout());
    assert!(
        matches!(error, Error::Timeout { operation: "daemon info", after } if after == Duration::from_millis(100))
    );
}

#[tokio::test]
async fn long_operations_have_their_own_deadline() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/daemon/diagnostics",
            Reply::json(200, &json!({})).delayed(Duration::from_millis(500)),
        )
        .start();
    let client = Client::builder()
        .with_socket(daemon.socket())
        .with_long_operation_timeout(Duration::from_millis(100))
        .connect()
        .await
        .unwrap();

    let error = client.daemon().diagnostics().await.unwrap_err();
    assert!(
        matches!(error, Error::Timeout { operation: "daemon diagnostics", after } if after == Duration::from_millis(100))
    );
}

#[tokio::test]
async fn answers_that_are_not_json_are_decoding_errors() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/daemon/info",
            Reply::raw(200, "application/json", "<html>"),
        )
        .start();
    let client = connect(&daemon).await;

    let error = client.daemon().info().await.unwrap_err();
    assert!(
        matches!(
            error,
            Error::Decode {
                operation: "daemon info",
                ..
            }
        ),
        "{error}"
    );
}

#[tokio::test]
async fn daemon_errors_keep_their_code_and_operation() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/sandbox/job-42/exec/deadbeef",
            Reply::json(409, &json!({ "code": "exec_not_running", "message": "exec \"deadbeef\" is no longer running" })),
        )
        .start();
    let client = connect(&daemon).await;

    let error = client
        .sandbox("job-42")
        .exec_inspect("deadbeef")
        .await
        .unwrap_err();
    assert!(error.is_conflict() && error.is_transient());
    assert_eq!(error.api_code(), Some("exec_not_running"));
    let api = error.api().unwrap();
    assert_eq!(api.operation(), "execution state");
    assert_eq!(api.message(), "exec \"deadbeef\" is no longer running");
}

#[tokio::test]
async fn settings_are_read_and_written() {
    let setting = json!({ "key": "sandbox.disk.dockerVolume", "value": "11g", "default": "10g", "source": "override", "type": "string" });
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/daemon/settings/sandbox.disk.dockerVolume",
            Reply::json(200, &setting),
        )
        .route(
            "DELETE",
            "/daemon/settings/sandbox.disk.dockerVolume",
            Reply::empty(204),
        )
        .route(
            "GET",
            "/daemon/settings",
            Reply::json(200, &json!({ "settings": [setting] })),
        )
        .start();
    let client = connect(&daemon).await;

    let updated = client
        .daemon()
        .set_setting("sandbox.disk.dockerVolume", "11g")
        .await
        .unwrap();
    assert_eq!(updated.source.as_deref(), Some("override"));
    assert_eq!(
        daemon
            .request("POST", "/daemon/settings/sandbox.disk.dockerVolume")
            .json(),
        json!({ "value": "11g" })
    );
    client
        .daemon()
        .unset_setting("sandbox.disk.dockerVolume")
        .await
        .unwrap();
    assert_eq!(client.daemon().settings().await.unwrap().len(), 1);
    assert_eq!(
        daemon.request("GET", "/daemon/settings").query()["all"],
        "true"
    );
}

#[tokio::test]
async fn log_levels_are_set_per_target() {
    let daemon = FakeDaemon::builder()
        .route("POST", "/daemon/loglevel/proxy", Reply::empty(204))
        .start();
    let client = connect(&daemon).await;

    client
        .daemon()
        .set_log_level("proxy", "debug")
        .await
        .unwrap();
    assert_eq!(
        daemon.request("POST", "/daemon/loglevel/proxy").json(),
        json!({ "level": "debug" })
    );
}

#[tokio::test]
async fn daemon_state_is_read() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/daemon/settings/sandbox.disk.dockerVolume",
            Reply::json(
                200,
                &json!({ "key": "sandbox.disk.dockerVolume", "value": "10g", "default": "10g" }),
            ),
        )
        .route(
            "GET",
            "/daemon/loglevel",
            Reply::json(200, &json!({ "general": "info", "proxy": "debug" })),
        )
        .route(
            "GET",
            "/daemon/disk-usage",
            Reply::json(
                200,
                &json!({ "summaries": [{ "type": "images", "total_count": 3, "active": 1, "size": 30, "reclaimable": 20 }] }),
            ),
        )
        .route(
            "GET",
            "/daemon/debug/state",
            Reply::json(200, &json!({ "sandboxes": [] })),
        )
        .route(
            "GET",
            "/daemon/diagnostics",
            Reply::json(200, &json!({ "checks": ["socket"] })),
        )
        .start();
    let client = connect(&daemon).await;
    let api = client.daemon();

    assert!(api.health().await.unwrap().is_healthy());
    assert_eq!(
        api.setting("sandbox.disk.dockerVolume")
            .await
            .unwrap()
            .value,
        "10g"
    );
    assert_eq!(api.log_levels().await.unwrap()["proxy"], "debug");
    let usage = api.disk_usage().await.unwrap();
    assert_eq!(usage.summaries[0].resource_type, "images");
    assert_eq!(usage.summaries[0].reclaimable, 20);
    assert_eq!(api.debug_state().await.unwrap(), json!({ "sandboxes": [] }));
    assert_eq!(
        api.diagnostics().await.unwrap(),
        json!({ "checks": ["socket"] })
    );
}

/// Fails to compile unless `T` can be stored and shared across tasks.
fn assert_shareable<T: Clone + Send + Sync + 'static>() {}

/// Fails to compile unless `T` can cross threads, as errors returned by tasks do.
fn assert_thread_safe<T: Send + Sync + 'static>() {}

/// Fails to compile unless the future can run on a multi-threaded runtime.
fn assert_send<T: Send>(_: &T) {}

#[test]
fn handles_are_owned_and_shareable() {
    assert_shareable::<Client>();
    assert_shareable::<Daemon>();
    assert_shareable::<Sandboxes>();
    assert_shareable::<Sandbox>();
    assert_shareable::<Images>();
    assert_shareable::<Events>();
    assert_shareable::<Policy>();
    assert_shareable::<Mcp>();
    assert_shareable::<Prompts>();
    assert_shareable::<SessionGuard>();
    assert_shareable::<ExecControl>();
    assert_thread_safe::<Error>();
}

#[tokio::test]
async fn operations_are_send_futures() {
    let daemon = FakeDaemon::builder().start();
    let client = connect(&daemon).await;
    let sandbox = client.sandbox("job-42");
    let spec = ExecSpec::new(["true"]);

    assert_send(&sandbox.exec(&spec));
    assert_send(&sandbox.ensure_running());
    assert_send(&sandbox.write_file("/tmp/a", "a"));
    assert_send(&sandbox.upload_dir("/tmp", "/tmp"));
    assert_send(&sandbox.allow_hosts(["example.com"]));
    assert_send(&client.sandboxes().list());
    assert_send(&client.events().subscribe());
    assert_send(&client.images().pull("x"));

    // A handle moves into a task without borrowing the client.
    let task = tokio::spawn(async move { sandbox.name().to_owned() });
    assert_eq!(task.await.unwrap(), "job-42");
}
