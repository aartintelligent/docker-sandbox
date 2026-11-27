//! Sandbox operations against a fake daemon: listing, creation, life cycle,
//! ports, credentials, files, sessions and SSH.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

mod support;

use std::time::Duration;

use docker_sandbox::Error;
use docker_sandbox::sandbox::{
    CreateEvent, CreateRequest, CredentialsUpdate, CustomSecret, DeleteOptions, PortBinding,
    PullPolicy, SandboxState, Secret, ServiceCredential, SkillsMode, StartOptions, Workspace,
};
use serde_json::json;
use support::{FakeDaemon, Reply, connect, sandbox_json, tar_entries, tar_with_file};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A creation template as captured from the CLI, with an unknown field.
fn create_template() -> serde_json::Value {
    json!({
        "name": "{{name}}",
        "agent": "shell",
        "kits": ["docker.io/sbx/shell@sha256:f3c1"],
        "kit_artifacts": [{ "manifest": { "name": "shell" } }],
        "pull_policy": "missing",
        "unknown_daemon_field": 1
    })
}

/// A creation request for `job-42`.
fn request() -> CreateRequest {
    CreateRequest::from_template(create_template(), "job-42").unwrap()
}

#[tokio::test]
async fn sandboxes_are_decoded_tolerantly() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/sandbox",
            Reply::json(
                200,
                &json!([
                    sandbox_json("job-1", "running"),
                    sandbox_json("job-2", "paused")
                ]),
            ),
        )
        .start();
    let client = connect(&daemon).await;

    let sandboxes = client.sandboxes().list().await.unwrap();
    assert_eq!(sandboxes.len(), 2);
    assert!(sandboxes[0].is_running());
    assert_eq!(sandboxes[1].status, SandboxState::Other("paused".into()));
    assert_eq!(sandboxes[0].extra["future_field"], json!({ "kept": true }));
    assert_eq!(
        daemon.request("GET", "/sandbox").query()["include_stopped_at"],
        "true"
    );
}

#[tokio::test]
async fn missing_sandboxes_are_reported_as_not_found() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/sandbox/ghost",
            Reply::json(404, &json!({ "message": "sandbox \"ghost\" not found" })),
        )
        .start();
    let client = connect(&daemon).await;

    let error = client.sandboxes().get("ghost").await.unwrap_err();
    assert!(error.is_not_found());
    assert!(error.to_string().contains("sandbox \"ghost\" not found"));
    assert!(client.sandboxes().find("ghost").await.unwrap().is_none());
    assert!(!client.sandbox("ghost").exists().await.unwrap());
}

#[tokio::test]
async fn create_resolves_the_progress_stream() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox",
            Reply::ndjson(&[
                json!({ "progress": "   ✓ assembled kit image ready (cached)\n" }),
                // A line this crate does not know is skipped.
                json!({ "notice": "kit cache is cold" }),
                json!({ "result": { "id": "abc", "name": "job-42", "agent": "shell", "status": "running", "workspace": "",
                                    "mcp": { "state": "ready", "static": false, "port": 41447 } } }),
            ]),
        )
        .start();
    let client = connect(&daemon).await;

    let created = client
        .sandboxes()
        .create(
            &request()
                .with_memory("4g")
                .with_cpus(2)
                .with_workspace("/srv/work")
                .with_envs([("A", "1"), ("B", "2")])
                .with_env("C", "3")
                .with_pull_policy(PullPolicy::Never)
                .with_ssh_agent_socket("/run/ssh-agent.sock")
                .with_static_mcp_servers(["docs"]),
        )
        .await
        .unwrap();
    assert_eq!(created.id, "abc");
    assert_eq!(created.mcp.unwrap().port, Some(41447));

    let sent = daemon.request("POST", "/sandbox");
    assert_eq!(sent.headers["accept"], "application/x-ndjson");
    let body = sent.json();
    assert_eq!(body["name"], "job-42");
    assert_eq!(body["memory"], "4g");
    assert_eq!(body["cpus"], 2);
    assert_eq!(body["workspace"], "/srv/work");
    assert_eq!(body["environment"], json!({ "A": "1", "B": "2", "C": "3" }));
    assert_eq!(body["pull_policy"], "never");
    assert_eq!(body["ssh_agent_socket_path"], "/run/ssh-agent.sock");
    // What `sbx create --static-mcp docs` sends (`create-s-static-mcp`).
    assert_eq!(body["mcp"], json!({ "servers": ["docs"], "static": true }));
    assert_eq!(
        body["unknown_daemon_field"], 1,
        "unknown template fields are forwarded"
    );
}

/// A credential a kit declares and no bindings file authorizes stays
/// withheld: named at creation, and again in the answer to a value pushed for
/// its service, which the proxy does not inject (`create-wc`,
/// `push-wc-credential` of the `kits` suite).
#[tokio::test]
async fn withheld_credentials_are_reported() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox",
            Reply::ndjson(&[
                json!({ "result": { "id": "abc", "name": "job-42", "agent": "shell", "status": "running", "workspace": "", "withheld": ["example"] } }),
            ]),
        )
        .route(
            "POST",
            "/sandbox/job-42/credentials",
            Reply::json(
                200,
                &json!({ "credential_source_updates_applied": true, "message": "Credentials updated", "withheld": ["example"] }),
            ),
        )
        .start();
    let client = connect(&daemon).await;

    let created = client.sandboxes().create(&request()).await.unwrap();
    assert_eq!(created.withheld, ["example"]);
    assert!(created.extra.is_empty(), "withheld is a typed field");

    let outcome = client
        .sandbox("job-42")
        .push_service_secret("example", "dummy")
        .await
        .unwrap();
    assert_eq!(outcome.withheld, ["example"]);
    assert_eq!(outcome.credential_source_updates_applied, Some(true));
    assert!(outcome.extra.is_empty());
}

#[tokio::test]
async fn create_reports_in_band_failures() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox",
            Reply::ndjson(&[
                json!({ "progress": "   → assembling kit image from 2 kits\n" }),
                json!({ "error": "image is not cached locally" }),
            ]),
        )
        .start();
    let client = connect(&daemon).await;

    let error = client.sandboxes().create(&request()).await.unwrap_err();
    assert!(
        matches!(error, Error::Failed { operation: "sandbox creation", ref message } if message == "image is not cached locally")
    );
}

#[tokio::test]
async fn create_without_outcome_is_an_error() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox",
            Reply::ndjson(&[json!({ "progress": "starting" })]),
        )
        .start();
    let client = connect(&daemon).await;

    assert!(matches!(
        client.sandboxes().create(&request()).await,
        Err(Error::Failed { .. })
    ));
}

#[tokio::test]
async fn create_stream_yields_progress_then_outcome() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox",
            Reply::ndjson(&[
                json!({ "progress": "   → assembling kit image from 2 kits\n" }),
                json!({ "progress": "   ✓ assembled kit image (3 layers)\n" }),
                json!({ "result": { "id": "abc", "name": "job-42", "agent": "shell", "status": "running" } }),
            ]),
        )
        .start();
    let client = connect(&daemon).await;

    let mut stream = client.sandboxes().create_stream(&request()).await.unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.unwrap());
    }

    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0],
        CreateEvent::Progress("   → assembling kit image from 2 kits".into())
    );
    assert!(matches!(&events[2], CreateEvent::Result(created) if created.id == "abc"));
}

#[tokio::test]
async fn deletion_is_guarded_by_the_sandbox_identifier() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/sandbox/job-42",
            Reply::json(200, &sandbox_json("job-42", "stopped")),
        )
        .route(
            "DELETE",
            "/sandbox/job-42",
            Reply::json(200, &json!({ "message": "sandbox \"job-42\" deleted" })),
        )
        .start();
    let client = connect(&daemon).await;
    let sandbox = client.sandbox("job-42");

    sandbox.force_delete().await.unwrap();
    let query = daemon.request("DELETE", "/sandbox/job-42").query();
    assert_eq!(query["expected_id"], "11765de3-dd25-4a0f-810f-af8f3bccc1ca");
    assert_eq!(query["force"], "true");
    assert_eq!(
        daemon.requests().len(),
        2,
        "the identifier is fetched first"
    );

    sandbox
        .delete_with(&DeleteOptions::default().with_expected_id("known-id"))
        .await
        .unwrap();
    let last = daemon.requests().pop().unwrap();
    assert_eq!(last.query()["expected_id"], "known-id");
    assert_eq!(last.query()["force"], "false");
    assert_eq!(
        daemon.requests().len(),
        3,
        "a known identifier is not fetched"
    );
}

#[tokio::test]
async fn life_cycle_requests_use_the_cli_bodies() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/stop",
            Reply::json(200, &sandbox_json("job-42", "stopped")),
        )
        .route(
            "POST",
            "/sandbox/job-42/save",
            Reply::json(
                201,
                &json!({ "image_id": "sha256:1", "tag": "docker.io/library/snap:v1" }),
            ),
        )
        .route(
            "PUT",
            "/sandbox/job-42/working-directory",
            Reply::json(200, &json!({ "message": "ok" })),
        )
        .route(
            "POST",
            "/sandbox/job-42/mounts",
            Reply::json(200, &json!({ "message": "Allowed" })),
        )
        .start();
    let client = connect(&daemon).await;
    let sandbox = client.sandbox("job-42");

    assert_eq!(sandbox.stop().await.unwrap().status, SandboxState::Stopped);
    assert_eq!(
        daemon.request("POST", "/sandbox/job-42/stop").json(),
        json!({})
    );
    let saved = sandbox.save("snap:v1").await.unwrap();
    assert_eq!(saved.tag, "docker.io/library/snap:v1");
    assert_eq!(
        daemon.request("POST", "/sandbox/job-42/save").json(),
        json!({ "tag": "snap:v1" })
    );
    sandbox.set_working_dir("/work").await.unwrap();
    assert_eq!(
        daemon
            .request("PUT", "/sandbox/job-42/working-directory")
            .json(),
        json!({ "working_dir": "/work" })
    );
    sandbox.allow_host_path("/srv/data").await.unwrap();
    assert_eq!(
        daemon.request("POST", "/sandbox/job-42/mounts").json(),
        json!({ "host_path": "/srv/data" })
    );
}

#[tokio::test]
async fn ports_round_trip() {
    let bound = json!([{ "host_ip": "127.0.0.1", "host_port": 18080, "protocol": "tcp4", "sandbox_port": 8080 }]);
    let daemon = FakeDaemon::builder()
        .route("POST", "/sandbox/job-42/ports", Reply::json(200, &bound))
        .start();
    let client = connect(&daemon).await;

    let published = client
        .sandbox("job-42")
        .publish(&[PortBinding::tcp(8080, 18080)])
        .await
        .unwrap();

    assert_eq!(published[0].protocol.as_deref(), Some("tcp4"));
    assert_eq!(
        daemon.request("POST", "/sandbox/job-42/ports").json(),
        json!([{ "sandbox_port": 8080, "host_port": 18080, "host_ip": "127.0.0.1", "protocol": "tcp" }])
    );
}

#[tokio::test]
async fn credentials_are_pushed_in_the_daemon_format() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/credentials",
            Reply::json(200, &json!({ "message": "Credentials updated" })),
        )
        .start();
    let client = connect(&daemon).await;

    let update = CredentialsUpdate::default().with_custom_secret(
        CustomSecret::new(["api.example.com"], "sbx-cs-1", "dummy").with_env_name("API_KEY"),
    );
    let outcome = client
        .sandbox("job-42")
        .push_credentials(&update)
        .await
        .unwrap();
    assert_eq!(outcome.message.as_deref(), Some("Credentials updated"));
    assert!(outcome.withheld.is_empty());
    assert_eq!(
        daemon.request("POST", "/sandbox/job-42/credentials").json(),
        json!({ "custom_secrets": [{ "env_name": "API_KEY", "sentinel": "sbx-cs-1", "target": "api.example.com", "targets": ["api.example.com"], "value": "dummy" }] })
    );
}

#[tokio::test]
async fn archives_are_sent_as_tar_with_an_encoded_path() {
    let daemon = FakeDaemon::builder()
        .route(
            "PUT",
            "/sandbox/job-42/files",
            Reply::json(200, &json!({ "message": "files copied" })),
        )
        .start();
    let client = connect(&daemon).await;

    client
        .sandbox("job-42")
        .upload_archive("/home/agent/my dir", &b"tar-bytes"[..])
        .await
        .unwrap();

    let sent = daemon.request("PUT", "/sandbox/job-42/files");
    assert_eq!(sent.headers["content-type"], "application/x-tar");
    assert_eq!(sent.query()["path"], "/home/agent/my dir");
    assert_eq!(sent.body, b"tar-bytes");
}

#[tokio::test]
async fn files_are_written_into_their_parent_directory() {
    let daemon = FakeDaemon::builder()
        .route(
            "PUT",
            "/sandbox/job-42/files",
            Reply::json(200, &json!({ "message": "files copied" })),
        )
        .start();
    let client = connect(&daemon).await;
    let sandbox = client.sandbox("job-42");

    sandbox
        .write_file_with_mode("/work/bin/run.sh", "echo ok\n", 0o755)
        .await
        .unwrap();
    let sent = daemon.request("PUT", "/sandbox/job-42/files");
    assert_eq!(sent.query()["path"], "/work/bin");
    assert_eq!(
        tar_entries(&sent.body),
        [("run.sh".to_owned(), 0o755, b"echo ok\n".to_vec())]
    );

    let invalid = sandbox.write_file("relative.txt", "x").await.unwrap_err();
    assert!(matches!(invalid, Error::InvalidRequest(_)));
}

#[tokio::test]
async fn files_and_directories_are_read_back() {
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join("out/logs")).unwrap();
    std::fs::write(source.path().join("out/logs/run.log"), "done\n").unwrap();
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_dir_all("out", source.path().join("out"))
        .unwrap();
    let directory = builder.into_inner().unwrap();

    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/sandbox/job-42/files",
            Reply::raw(
                200,
                "application/x-tar",
                tar_with_file("app.toml", b"a = 1\n"),
            ),
        )
        .start();
    let client = connect(&daemon).await;
    let contents = client
        .sandbox("job-42")
        .read_file("/etc/app.toml")
        .await
        .unwrap();
    assert_eq!(&contents[..], b"a = 1\n");
    assert_eq!(
        daemon.request("GET", "/sandbox/job-42/files").query()["path"],
        "/etc/app.toml"
    );

    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/sandbox/job-42/files",
            Reply::raw(200, "application/x-tar", directory),
        )
        .start();
    let client = connect(&daemon).await;
    let target = tempfile::tempdir().unwrap();
    let sandbox = client.sandbox("job-42");
    sandbox
        .download_dir("/work/out", target.path().join("results"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(target.path().join("results/out/logs/run.log")).unwrap(),
        "done\n"
    );
    let not_a_file = sandbox.read_file("/work/out").await.unwrap_err();
    assert!(matches!(not_a_file, Error::Archive { .. }));
}

#[tokio::test]
async fn host_directories_are_uploaded_under_their_name() {
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join("repo/src")).unwrap();
    std::fs::write(source.path().join("repo/src/main.rs"), "fn main() {}\n").unwrap();
    let daemon = FakeDaemon::builder()
        .route(
            "PUT",
            "/sandbox/job-42/files",
            Reply::json(200, &json!({ "message": "files copied" })),
        )
        .start();
    let client = connect(&daemon).await;

    client
        .sandbox("job-42")
        .upload_dir(source.path().join("repo"), "/home/agent")
        .await
        .unwrap();
    let sent = daemon.request("PUT", "/sandbox/job-42/files");
    assert_eq!(sent.query()["path"], "/home/agent");
    let paths: Vec<String> = tar_entries(&sent.body).into_iter().map(|e| e.0).collect();
    assert!(paths.contains(&"repo/src/main.rs".to_owned()), "{paths:?}");

    let missing = client
        .sandbox("job-42")
        .upload_dir(source.path().join("absent"), "/home/agent")
        .await
        .unwrap_err();
    assert!(matches!(missing, Error::File { .. }));
}

#[tokio::test]
async fn sessions_are_reopened_until_the_last_guard_is_dropped() {
    // The fake daemon closes every session immediately, forcing reconnections.
    let daemon = FakeDaemon::builder()
        .route("GET", "/sandbox/job-42/session", Reply::empty(200))
        .start();
    let client = connect(&daemon).await;
    let sessions = || {
        daemon
            .requests()
            .iter()
            .filter(|r| r.path() == "/sandbox/job-42/session")
            .count()
    };

    let guard = client.sandbox("job-42").hold_session().await.unwrap();
    let shared = guard.clone();
    assert_eq!(shared.sandbox(), "job-42");
    drop(guard);
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    assert!(
        sessions() >= 2,
        "a clone keeps the session ({} request)",
        sessions()
    );

    drop(shared);
    tokio::time::sleep(Duration::from_millis(700)).await;
    let after_drop = sessions();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        after_drop,
        sessions(),
        "session kept re-opening after the last guard was dropped"
    );
}

#[tokio::test]
async fn ensure_running_follows_the_cli_sequence() {
    let daemon = FakeDaemon::builder()
        .route("GET", "/sandbox/job-42/session", Reply::empty(200))
        .route(
            "POST",
            "/sandbox/job-42/credentials",
            Reply::json(200, &json!({ "message": "Credentials updated" })),
        )
        .route(
            "POST",
            "/sandbox/job-42/start",
            Reply::json(200, &sandbox_json("job-42", "running")),
        )
        .start();
    let client = connect(&daemon).await;

    let _running = client.sandbox("job-42").ensure_running().await.unwrap();
    let paths: Vec<String> = daemon
        .requests()
        .iter()
        .map(|r| r.path().to_owned())
        .take(3)
        .collect();
    assert_eq!(
        paths,
        [
            "/sandbox/job-42/session",
            "/sandbox/job-42/credentials",
            "/sandbox/job-42/start"
        ]
    );
}

#[tokio::test]
async fn holding_a_session_on_a_missing_sandbox_fails() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/sandbox/ghost/session",
            Reply::json(404, &json!({ "message": "sandbox \"ghost\" not found" })),
        )
        .start();
    let client = connect(&daemon).await;

    assert!(
        client
            .sandbox("ghost")
            .hold_session()
            .await
            .unwrap_err()
            .is_not_found()
    );
}

#[tokio::test]
async fn ssh_tunnel_asks_for_the_ssh_protocol() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/ssh",
            Reply::upgrade("ssh", &[], b"SSH-2.0-Go\r\n".to_vec()),
        )
        .start();
    let client = connect(&daemon).await;

    let mut tunnel = client.sandbox("job-42").ssh_tunnel().await.unwrap();
    let mut banner = vec![0_u8; 12];
    tunnel.read_exact(&mut banner).await.unwrap();
    assert_eq!(banner, b"SSH-2.0-Go\r\n");
    drop(tunnel);

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        daemon.request("POST", "/sandbox/job-42/ssh").headers["upgrade"],
        "ssh"
    );
}

#[tokio::test]
async fn service_secrets_are_pushed_as_the_cli_scopes_them() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/credentials",
            Reply::json(200, &json!({ "message": "Credentials updated" })),
        )
        .start();
    let client = connect(&daemon).await;
    let sandbox = client.sandbox("job-42");

    sandbox
        .push_service_secret("github", "dummy")
        .await
        .unwrap();
    sandbox.remove_service_secret("github").await.unwrap();
    let bodies: Vec<serde_json::Value> = daemon
        .requests()
        .iter()
        .map(support::Recorded::json)
        .collect();
    assert_eq!(
        bodies,
        [
            json!({ "credential_values": [{ "service": "github", "value": "dummy" }],
                    "credential_sources": { "github": { "source": "SANDBOX_STORED_CREDENTIAL_JOB_42_GITHUB", "type": "env" } } }),
            json!({ "credential_values": [{ "service": "github", "value": "" }],
                    "removed_credential_sources": ["github"] }),
        ]
    );
}

#[tokio::test]
async fn existence_is_told_apart_from_failures() {
    for (status, expected) in [(200, Some(true)), (404, Some(false)), (500, None)] {
        let body = if status == 200 {
            sandbox_json("job-42", "running")
        } else {
            json!({ "message": "sandbox inspection failed" })
        };
        let daemon = FakeDaemon::builder()
            .route("GET", "/sandbox/job-42", Reply::json(status, &body))
            .start();
        let client = connect(&daemon).await;

        let exists = client.sandbox("job-42").exists().await;
        assert_eq!(exists.ok(), expected, "inspection answered {status}");
        let found = client.sandboxes().find("job-42").await;
        assert_eq!(
            found.ok().map(|sandbox| sandbox.is_some()),
            expected,
            "inspection answered {status}"
        );
    }
}

#[tokio::test]
async fn deletion_mounts_and_metrics_use_the_cli_requests() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/sandbox/job-42",
            Reply::json(200, &sandbox_json("job-42", "stopped")),
        )
        .route(
            "DELETE",
            "/sandbox/job-42",
            Reply::json(200, &json!({ "message": "sandbox \"job-42\" deleted" })),
        )
        .route(
            "DELETE",
            "/sandbox/job-42/mounts",
            Reply::json(200, &json!({ "message": "Revoked" })),
        )
        .route(
            "GET",
            "/sandbox/job-42/debug/vm",
            Reply::raw(200, "text/plain", "vm_up 1\n"),
        )
        .start();
    let client = connect(&daemon).await;
    let sandbox = client.sandbox("job-42");

    sandbox.delete().await.unwrap();
    let query = daemon.request("DELETE", "/sandbox/job-42").query();
    assert_eq!(query["expected_id"], "11765de3-dd25-4a0f-810f-af8f3bccc1ca");
    assert_eq!(query["force"], "false", "a plain deletion is not forced");

    sandbox.revoke_host_path("/srv/data").await.unwrap();
    assert_eq!(
        daemon.request("DELETE", "/sandbox/job-42/mounts").json(),
        json!({ "host_path": "/srv/data" })
    );

    assert_eq!(sandbox.vm_metrics().await.unwrap(), "vm_up 1\n");
    let metrics = daemon.request("GET", "/sandbox/job-42/debug/vm");
    assert_eq!(metrics.query()["route"], "/metrics");
    assert_eq!(metrics.headers["accept"], "*/*");
}

#[tokio::test]
async fn start_options_forward_the_ssh_agent() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/start",
            Reply::json(200, &sandbox_json("job-42", "running")),
        )
        .start();
    let client = connect(&daemon).await;

    let options = StartOptions::default().with_ssh_agent_socket("/run/ssh-agent.sock");
    let started = client.sandbox("job-42").start_with(&options).await.unwrap();
    assert!(started.is_running());
    assert_eq!(
        daemon.request("POST", "/sandbox/job-42/start").json(),
        json!({ "ssh_agent_socket_path": "/run/ssh-agent.sock" })
    );
}

#[tokio::test]
async fn ports_are_listed_unpublished_and_replaced() {
    let bound = json!([{ "host_ip": "0.0.0.0", "host_port": 15353, "protocol": "udp4", "sandbox_port": 5353 }]);
    let daemon = FakeDaemon::builder()
        .route("GET", "/sandbox/job-42/ports", Reply::json(200, &bound))
        .route(
            "POST",
            "/sandbox/job-42/ports/unpublish",
            Reply::json(200, &json!({ "message": "ok" })),
        )
        .route(
            "POST",
            "/sandbox/job-42/ports/replace-desired",
            Reply::json(200, &json!({ "message": "ok" })),
        )
        .start();
    let client = connect(&daemon).await;
    let sandbox = client.sandbox("job-42");
    let dns = PortBinding::udp(5353, 15353).with_host_ip("0.0.0.0");

    let ports = sandbox.ports().await.unwrap();
    assert_eq!(ports[0].sandbox_port, 5353);
    assert_eq!(ports[0].protocol.as_deref(), Some("udp4"));

    sandbox.unpublish(std::slice::from_ref(&dns)).await.unwrap();
    assert_eq!(
        daemon
            .request("POST", "/sandbox/job-42/ports/unpublish")
            .json(),
        json!([{ "sandbox_port": 5353, "host_port": 15353, "host_ip": "0.0.0.0", "protocol": "udp" }])
    );

    let web = PortBinding::tcp(8080, 18080);
    sandbox.replace_desired_port(&dns, &web).await.unwrap();
    let body = daemon
        .request("POST", "/sandbox/job-42/ports/replace-desired")
        .json();
    assert_eq!(body["old"]["protocol"], "udp");
    assert_eq!(body["new"]["host_port"], 18080);
}

#[tokio::test]
async fn ssh_tunnels_carry_bytes_both_ways() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/ssh",
            Reply::upgrade("ssh", &[], b"SSH-2.0-Go\r\n".to_vec()),
        )
        .start();
    let client = connect(&daemon).await;

    let mut tunnel = client.sandbox("job-42").ssh_tunnel().await.unwrap();
    assert_eq!(format!("{tunnel:?}"), "SshTunnel { .. }");
    tunnel.write_all(b"SSH-2.0-OpenSSH\r\n").await.unwrap();
    tunnel.flush().await.unwrap();
    tunnel.shutdown().await.unwrap();
    let mut banner = Vec::new();
    tunnel.read_to_end(&mut banner).await.unwrap();
    assert_eq!(banner, b"SSH-2.0-Go\r\n");

    assert_eq!(
        daemon.request("POST", "/sandbox/job-42/ssh").upgrade_input,
        b"SSH-2.0-OpenSSH\r\n"
    );
}

#[test]
fn secrets_never_show_in_debug_output() {
    let secret = Secret::new("dummy-value");
    assert_eq!(secret.expose(), "dummy-value");
    assert_eq!(format!("{secret:?}"), "Secret(***)");
    assert_eq!(Secret::from("dummy-value".to_owned()), secret);

    let cleared = ServiceCredential::cleared("github");
    assert_eq!(cleared.value.expose(), "");
    assert!(!format!("{:?}", ServiceCredential::new("github", "dummy-value")).contains("dummy"));
}

#[test]
fn empty_credential_updates_are_recognised() {
    assert!(CredentialsUpdate::default().is_empty());
    let update = CredentialsUpdate::default().with_custom_secret(CustomSecret::new(
        ["api.example.com"],
        "sbx-cs-1",
        "dummy",
    ));
    assert!(!update.is_empty());
}

#[test]
fn sandbox_states_print_as_the_daemon_spells_them() {
    for (state, wire) in [
        (SandboxState::Running, "running"),
        (SandboxState::Stopped, "stopped"),
        (SandboxState::Other("paused".into()), "paused"),
    ] {
        assert_eq!(state.as_str(), wire);
        assert_eq!(
            format!("{state:>8}"),
            format!("{wire:>8}"),
            "padding is honoured"
        );
        assert_eq!(String::from(state), wire);
    }
}

#[tokio::test]
async fn creation_streams_reject_malformed_lines() {
    for line in ["not json", r#"{"result":{"id":42}}"#] {
        let daemon = FakeDaemon::builder()
            .route(
                "POST",
                "/sandbox",
                Reply::raw(200, "application/x-ndjson", format!("{line}\n")),
            )
            .start();
        let client = connect(&daemon).await;

        let error = client.sandboxes().create(&request()).await.unwrap_err();
        assert!(
            matches!(
                error,
                Error::Decode {
                    operation: "sandbox creation",
                    ..
                }
            ),
            "{line}: {error}"
        );
    }
}

#[tokio::test]
async fn a_held_session_reports_its_connection() {
    // The daemon keeps the session open well past the assertions.
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/sandbox/job-42/session",
            Reply::empty(200).with_body_after(Duration::from_secs(5)),
        )
        .start();
    let client = connect(&daemon).await;

    let guard = client.sandbox("job-42").hold_session().await.unwrap();
    assert!(guard.is_connected());
    guard.release();
}

#[tokio::test]
async fn sessions_end_once_the_sandbox_is_gone() {
    // The first session closes at once and cannot be re-opened: a daemon
    // error is retried, a missing sandbox ends the session.
    let daemon = FakeDaemon::builder()
        .route_sequence(
            "GET",
            "/sandbox/job-42/session",
            vec![
                Reply::raw(200, "text/plain", "keep-alive").truncated(),
                Reply::json(500, &json!({ "message": "session failed" })),
                Reply::json(404, &json!({ "message": "sandbox \"job-42\" not found" })),
            ],
        )
        .start();
    let client = connect(&daemon).await;

    let attempts = || {
        daemon
            .requests()
            .iter()
            .filter(|r| r.path() == "/sandbox/job-42/session")
            .count()
    };

    let guard = client.sandbox("job-42").hold_session().await.unwrap();
    // Re-opened after 0.5 s, then after 1 s more.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while attempts() < 3 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!guard.is_connected());
    // A further attempt would come 2 s after the last one.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(attempts(), 3, "no attempt after the sandbox is gone");
}

#[tokio::test]
async fn creation_options_use_the_fields_the_cli_sends() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox",
            Reply::ndjson(&[json!({ "result": { "id": "1", "name": "job-42", "agent": "shell", "status": "running" } })]),
        )
        .start();
    let client = connect(&daemon).await;

    let request = request()
        .with_workspace("/srv/project")
        .with_additional_workspace(Workspace::new("/srv/docs").read_only())
        .with_additional_workspace(Workspace::new("/srv/data").at("/workspace/data"))
        .with_additional_workspace(Workspace::new("/srv/project/README.md").read_only())
        .with_port(PortBinding::tcp(8082, 18082))
        .with_skills(SkillsMode::Off)
        .with_profile("strict")
        .with_bindings_path("/home/agent/.config/sbx/credentials.yaml");
    client.sandboxes().create(&request).await.unwrap();

    // What `create-w-workspaces` and `create-s-options` of the `agents` suite
    // sent, the CLI rewriting `tcp` as `tcp4`.
    let body = daemon.request("POST", "/sandbox").json();
    assert_eq!(body["workspace"], "/srv/project");
    assert_eq!(
        body["additional_workspaces"],
        json!([
            { "dir": "/srv/docs", "read_only": true },
            { "dir": "/srv/data", "target": "/workspace/data" },
            { "dir": "/srv/project/README.md", "read_only": true }
        ])
    );
    assert_eq!(
        body["ports"],
        json!([{ "sandbox_port": 8082, "host_port": 18082, "host_ip": "127.0.0.1", "protocol": "tcp" }])
    );
    assert_eq!(body["skills"], "off");
    assert_eq!(body["profile"], "strict");
    assert_eq!(
        body["bindings_path"],
        "/home/agent/.config/sbx/credentials.yaml"
    );
    assert!(
        body.get("clone").is_none(),
        "a plain workspace is not cloned"
    );
}

#[tokio::test]
async fn a_saved_sandbox_is_the_template_of_a_new_one() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/save",
            Reply::json(201, &json!({ "image_id": "sha256:5a7e", "tag": "docker.io/library/job-42-snapshot:v1" })),
        )
        .route(
            "POST",
            "/sandbox",
            Reply::ndjson(&[json!({ "result": { "id": "2", "name": "job-43", "agent": "shell", "status": "running" } })]),
        )
        .start();
    let client = connect(&daemon).await;

    let saved = client
        .sandbox("job-42")
        .save("job-42-snapshot:v1")
        .await
        .unwrap();
    // The daemon answers with the normalized reference of the tag.
    assert_eq!(saved.tag, "docker.io/library/job-42-snapshot:v1");
    let mut from_saved = request()
        .with_template("job-42-snapshot:v1")
        .with_pull_policy(PullPolicy::Never);
    from_saved.name = "job-43".to_owned();
    client.sandboxes().create(&from_saved).await.unwrap();

    // What `create-b-from-template` of the `templates` suite sent for
    // `sbx create -t audit-snap:v1 --pull never`.
    let body = daemon.request("POST", "/sandbox").json();
    assert_eq!(body["template"], "job-42-snapshot:v1");
    assert_eq!(body["pull_policy"], "never");
    assert!(request().template.is_none(), "no template by default");
    assert!(
        serde_json::to_value(request())
            .unwrap()
            .get("template")
            .is_none()
    );
}

#[test]
fn clone_mode_and_creation_options_round_trip() {
    let cloned = request().with_cloned_workspace("/srv/repo");
    let body = serde_json::to_value(&cloned).unwrap();
    assert_eq!(body["workspace"], "/srv/repo");
    assert_eq!(body["clone"], true);
    assert!(body.get("additional_workspaces").is_none());
    assert!(body.get("ports").is_none());

    let template = json!({
        "name": "{{name}}", "agent": "shell", "clone": true, "workspace": "/srv/repo",
        "skills": "readwrite", "additional_workspaces": [{ "dir": "/srv/docs", "read_only": true }]
    });
    let decoded = CreateRequest::from_template(template, "job-42").unwrap();
    assert!(decoded.clone);
    assert_eq!(decoded.skills, Some(SkillsMode::Readwrite));
    assert_eq!(
        decoded.additional_workspaces,
        [Workspace::new("/srv/docs").read_only()]
    );
    assert!(decoded.extra.is_empty(), "every field is known");
}

#[tokio::test]
async fn an_unknown_profile_is_refused_before_creation() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox",
            Reply::json(
                400,
                &json!({ "message": "profile \"zz-audit-none\" not found. Use an existing profile (sbx policy profile ls) or omit --profile" }),
            ),
        )
        .start();
    let client = connect(&daemon).await;

    let error = client
        .sandboxes()
        .create(&request().with_profile("zz-audit-none"))
        .await
        .unwrap_err();
    assert!(error.is_invalid_input(), "{error}");
}

#[tokio::test]
async fn custom_secrets_are_replaced_whole_or_removed_on_purpose() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/credentials",
            Reply::json(200, &json!({ "message": "Credentials updated" })),
        )
        .start();
    let client = connect(&daemon).await;
    let sandbox = client.sandbox("job-42");

    sandbox
        .push_credentials(&CredentialsUpdate::default())
        .await
        .unwrap();
    sandbox
        .push_credentials(&CredentialsUpdate::default().without_custom_secrets())
        .await
        .unwrap();
    let bodies: Vec<serde_json::Value> = daemon
        .requests()
        .iter()
        .map(support::Recorded::json)
        .collect();
    // An empty update leaves the custom secrets of the sandbox; an empty list
    // removes them.
    assert_eq!(bodies, [json!({}), json!({ "custom_secrets": [] })]);

    assert!(CredentialsUpdate::default().is_empty());
    assert!(
        !CredentialsUpdate::default()
            .without_custom_secrets()
            .is_empty()
    );
    let decoded: CredentialsUpdate =
        serde_json::from_value(json!({ "custom_secrets": [] })).unwrap();
    assert_eq!(decoded.custom_secrets, Some(Vec::new()));
    let decoded: CredentialsUpdate = serde_json::from_value(json!({})).unwrap();
    assert_eq!(decoded.custom_secrets, None);
}
