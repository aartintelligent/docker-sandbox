//! NDJSON streams against a fake daemon: events, image pulls and approval
//! prompts.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

mod support;

use docker_sandbox::Error;
use docker_sandbox::event::EventKind;
use serde_json::json;
use support::{FakeDaemon, Reply, connect};

/// A replay, its end marker, then one live event.
fn event_lines() -> Reply {
    Reply::ndjson(&[
        json!({ "id": "1", "type": "policy.network", "action": "allowed", "sandbox_name": "old", "data": { "count": 4 } }),
        json!({ "id": "2", "type": "sync", "action": "complete" }),
        json!({ "id": "3", "type": "sandbox.lifecycle", "action": "created", "sandbox_name": "job-42", "seq": 1 }),
    ])
}

#[tokio::test]
async fn events_are_streamed_until_the_daemon_closes() {
    let daemon = FakeDaemon::builder()
        .route("GET", "/events", event_lines())
        .start();
    let client = connect(&daemon).await;

    let mut stream = client.events().subscribe().await.unwrap();
    let mut kinds = Vec::new();
    while let Some(event) = stream.next().await {
        kinds.push(event.unwrap().kind());
    }

    assert_eq!(
        kinds,
        [
            EventKind::PolicyNetwork,
            EventKind::Sync,
            EventKind::SandboxLifecycle
        ]
    );
    assert_eq!(
        daemon.request("GET", "/events").headers["accept"],
        "application/x-ndjson"
    );
}

#[tokio::test]
async fn live_subscriptions_skip_the_replay() {
    let daemon = FakeDaemon::builder()
        .route("GET", "/events", event_lines())
        .start();
    let client = connect(&daemon).await;

    let mut stream = client.events().subscribe_live().await.unwrap();
    let event = stream.next().await.unwrap().unwrap();
    assert!(event.is_about("job-42"));
    assert_eq!(event.action, "created");
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn pulls_follow_the_progress_to_its_outcome() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/docker/images/create",
            Reply::ndjson(&[
                json!({ "id": "f3c1", "status": "Downloading", "progressDetail": { "current": 10, "total": 20 } }),
                json!({ "id": "f3c1", "status": "Download complete", "progressDetail": { "hidecounts": true } }),
            ]),
        )
        .start();
    let client = connect(&daemon).await;

    client
        .images()
        .pull("docker.io/sbx/shell:latest")
        .await
        .unwrap();
    let query = daemon.request("POST", "/docker/images/create").query();
    assert_eq!(query["fromImage"], "docker.io/sbx/shell:latest");
    assert_eq!(query["allowCachedOnForbidden"], "true");

    let mut stream = client.images().pull_stream("x").await.unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.progress_detail.unwrap().total, Some(20));
}

#[tokio::test]
async fn pull_failures_reported_in_band_are_errors() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/docker/images/create",
            Reply::ndjson(&[
                json!({ "status": "Pulling from sbx/shell" }),
                json!({ "errorDetail": { "message": "manifest unknown" } }),
            ]),
        )
        .start();
    let client = connect(&daemon).await;

    let error = client.images().pull("sbx/shell:none").await.unwrap_err();
    assert!(
        matches!(error, Error::Failed { operation: "image pull", ref message } if message == "manifest unknown")
    );
}

#[tokio::test]
async fn prompts_are_streamed_and_answered() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/user-prompts",
            Reply::ndjson(&[
                json!({ "action": "synced" }),
                json!({ "action": "created", "id": "p1", "title": "Allow api.example.com?" }),
            ]),
        )
        .route("POST", "/user-prompts/p1/response", Reply::empty(204))
        .start();
    let client = connect(&daemon).await;

    let mut prompts = client.prompts().subscribe().await.unwrap();
    assert!(prompts.next().await.unwrap().unwrap().is_synced());
    let prompt = prompts.next().await.unwrap().unwrap();
    assert_eq!(prompt.id.as_deref(), Some("p1"));
    assert_eq!(prompt.extra["title"], "Allow api.example.com?");

    client.prompts().respond("p1", "allow-once").await.unwrap();
    assert_eq!(
        daemon.request("POST", "/user-prompts/p1/response").json(),
        json!({ "option_id": "allow-once" })
    );
}

#[tokio::test]
async fn late_lines_and_a_last_line_without_newline_are_decoded() {
    let body = concat!(
        r#"{"id":"1","type":"sandbox.ports","action":"published"}"#,
        "\n",
        r#"{"id":"2","type":"kit.cache","action":"pruned"}"#,
    );
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/events",
            Reply::raw(200, "application/x-ndjson", body)
                .with_body_after(std::time::Duration::from_millis(100)),
        )
        .start();
    let client = connect(&daemon).await;

    let mut stream = client.events().subscribe().await.unwrap();
    assert!(format!("{stream:?}").starts_with("EventStream { replaying: "));
    let mut kinds = Vec::new();
    while let Some(event) = stream.next().await {
        kinds.push(event.unwrap().kind());
    }
    assert_eq!(kinds, [EventKind::SandboxPorts, EventKind::Other]);
}

#[tokio::test]
async fn a_stream_cut_short_ends_with_an_error() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/docker/images/create",
            Reply::ndjson(&[json!({ "status": "Pulling" })]).truncated(),
        )
        .start();
    let client = connect(&daemon).await;

    let mut stream = client.images().pull_stream("sbx/shell").await.unwrap();
    assert!(format!("{stream:?}").starts_with("NdjsonStream { operation: \"image pull\""));
    assert!(stream.next().await.unwrap().is_ok());
    assert!(matches!(
        stream.next().await,
        Some(Err(Error::Http {
            operation: "image pull",
            ..
        }))
    ));
    assert!(stream.next().await.is_none(), "nothing follows the error");
}

#[tokio::test]
async fn opening_a_stream_times_out_with_the_operation_name() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/events",
            event_lines().delayed(std::time::Duration::from_millis(500)),
        )
        .start();
    let client = docker_sandbox::Client::builder()
        .with_socket(daemon.socket())
        .with_timeout(std::time::Duration::from_millis(100))
        .connect()
        .await
        .unwrap();

    let error = client.events().subscribe().await.unwrap_err();
    assert!(
        matches!(
            error,
            Error::Timeout {
                operation: "event stream",
                ..
            }
        ),
        "{error}"
    );
}
