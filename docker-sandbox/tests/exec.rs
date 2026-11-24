//! Command execution against a fake daemon: synchronous commands, interactive
//! executions over upgraded connections and agent sessions.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

mod support;

use std::time::Duration;

use docker_sandbox::Error;
use docker_sandbox::exec::{AgentSessionSpec, AttachSpec, ExecSpec, OutputStream, TerminalSize};
use serde_json::json;
use support::{FakeDaemon, Reply, connect};
use tokio::io::AsyncWriteExt;

/// How long the tests wait for an exit code.
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);

/// A stdcopy frame: `[stream, 0, 0, 0, size: u32 BE]` followed by `data`.
fn frame(stream: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![stream, 0, 0, 0];
    out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// The state of an execution that exited with `code`.
fn exited(code: i32) -> Reply {
    Reply::json(
        200,
        &json!({ "id": "e1", "running": false, "exit_code": code }),
    )
}

#[tokio::test]
async fn exec_uses_daemon_field_names() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec",
            Reply::json(
                200,
                &json!({ "exit_code": 3, "stdout": "out", "stderr": "err" }),
            ),
        )
        .start();
    let client = connect(&daemon).await;

    let spec = ExecSpec::shell("exit 3")
        .with_user("0")
        .with_working_dir("/tmp")
        .with_env("FOO", "bar");
    let output = client.sandbox("job-42").exec(&spec).await.unwrap();

    assert_eq!(output.exit_code, 3);
    assert!(!output.success());
    assert_eq!(
        daemon.request("POST", "/sandbox/job-42/exec").json(),
        json!({ "cmd": ["sh", "-c", "exit 3"], "user": "0", "working_dir": "/tmp", "env": { "FOO": "bar" } })
    );

    let failure = output.check().unwrap_err();
    assert_eq!(failure.to_string(), "command exited with code 3: err");
    assert!(matches!(failure, Error::CommandFailed(ref output) if output.stdout == "out"));
}

#[tokio::test]
async fn commands_have_their_own_deadline() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec",
            Reply::json(200, &json!({ "exit_code": 0 })).delayed(Duration::from_millis(500)),
        )
        .start();
    let client = connect(&daemon).await;

    let spec = ExecSpec::new(["sleep", "1"]).with_timeout(Duration::from_millis(100));
    let error = client.sandbox("job-42").exec(&spec).await.unwrap_err();
    assert!(matches!(
        error,
        Error::Timeout {
            operation: "command execution",
            ..
        }
    ));
    let sent = daemon.requests();
    assert!(
        sent.is_empty() || sent[0].json().get("timeout").is_none(),
        "the deadline is not sent"
    );
}

#[tokio::test]
async fn attach_demultiplexes_output_and_reports_the_exit_code() {
    let mut payload = frame(1, b"out\n");
    payload.extend(frame(2, b"err\n"));
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec/attach",
            Reply::upgrade("tcp", &[("sandboxes-exec-id", "e1")], payload),
        )
        .route("GET", "/sandbox/job-42/exec/e1", exited(3))
        .start();
    let client = connect(&daemon).await;

    let session = client
        .sandbox("job-42")
        .attach(&AttachSpec::new(["cat"]).with_user("0"))
        .await
        .unwrap();
    assert_eq!(session.id(), "e1");
    let output = session.collect("hello stdin", EXIT_TIMEOUT).await.unwrap();

    assert_eq!(output.stdout, b"out\n");
    assert_eq!(output.stderr, b"err\n");
    assert_eq!(output.exit_code, 3);

    let sent = daemon.request("POST", "/sandbox/job-42/exec/attach");
    assert_eq!(sent.headers["upgrade"], "tcp");
    assert_eq!(sent.headers["connection"].to_ascii_lowercase(), "upgrade");
    assert_eq!(sent.json(), json!({ "cmd": ["cat"], "user": "0" }));
    assert_eq!(
        sent.upgrade_input, b"hello stdin",
        "stdin is written then closed"
    );
}

#[tokio::test]
async fn terminal_output_is_passed_through_raw() {
    let raw = frame(1, b"framed-looking bytes");
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec/attach",
            Reply::upgrade("tcp", &[("sandboxes-exec-id", "e1")], raw.clone()),
        )
        .route("GET", "/sandbox/job-42/exec/e1", exited(0))
        .start();
    let client = connect(&daemon).await;

    let spec = AttachSpec::new(["bash"]).with_tty(TerminalSize::new(40, 120));
    let output = client
        .sandbox("job-42")
        .attach(&spec)
        .await
        .unwrap()
        .collect("", EXIT_TIMEOUT)
        .await
        .unwrap();

    assert_eq!(output.stdout, raw);
    assert!(output.stderr.is_empty() && output.success());
    let body = daemon.request("POST", "/sandbox/job-42/exec/attach").json();
    assert_eq!(body["tty"], true);
    assert_eq!(body["terminal_size"], json!({ "height": 40, "width": 120 }));
}

#[tokio::test]
async fn split_sessions_stream_chunks_and_resize() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec/attach",
            Reply::upgrade("tcp", &[("sandboxes-exec-id", "e1")], frame(1, b"ready\n")),
        )
        .route(
            "POST",
            "/sandbox/job-42/exec/e1/resize",
            Reply::json(200, &json!({ "message": "resize applied" })),
        )
        .route("GET", "/sandbox/job-42/exec/e1", exited(0))
        .start();
    let client = connect(&daemon).await;

    let session = client
        .sandbox("job-42")
        .attach(&AttachSpec::new(["bash"]))
        .await
        .unwrap();
    let (mut input, mut output, control) = session.split();

    let chunk = output.next().await.unwrap().unwrap();
    assert_eq!(
        (chunk.stream, chunk.data.as_ref()),
        (OutputStream::Stdout, &b"ready\n"[..])
    );
    control.resize(TerminalSize::new(50, 100)).await.unwrap();
    input.write_all(b"exit\n").await.unwrap();
    input.shutdown().await.unwrap();
    assert!(output.next().await.is_none());
    assert_eq!(control.wait(EXIT_TIMEOUT).await.unwrap(), 0);

    let resize = daemon.request("POST", "/sandbox/job-42/exec/e1/resize");
    assert_eq!(resize.query()["h"], "50");
    assert_eq!(resize.query()["w"], "100");
}

#[tokio::test]
async fn agent_sessions_default_to_a_terminal() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/agent/session",
            Reply::upgrade("tcp", &[("sandboxes-exec-id", "a1")], b"$ ".to_vec()),
        )
        .start();
    let client = connect(&daemon).await;

    let spec = AgentSessionSpec::default()
        .with_working_dir("/home/agent/repo")
        .with_envs([("MODE", "ci")]);
    let session = client.sandbox("job-42").agent_session(&spec).await.unwrap();
    assert_eq!(session.id(), "a1");
    drop(session);
    // The fake daemon records an upgraded request once the connection closes.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let body = daemon
        .request("POST", "/sandbox/job-42/agent/session")
        .json();
    assert_eq!(
        body,
        json!({ "tty": true, "terminal_size": { "height": 24, "width": 80 }, "working_dir": "/home/agent/repo", "env": { "MODE": "ci" } })
    );
}

#[tokio::test]
async fn refused_upgrades_and_missing_exec_ids_are_errors() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/stopped/exec/attach",
            Reply::json(
                409,
                &json!({ "message": "sandbox \"stopped\" is not running" }),
            ),
        )
        .route(
            "POST",
            "/sandbox/odd/exec/attach",
            Reply::upgrade("tcp", &[], Vec::new()),
        )
        .start();
    let client = connect(&daemon).await;

    let refused = client
        .sandbox("stopped")
        .attach(&AttachSpec::new(["true"]))
        .await
        .unwrap_err();
    assert!(refused.is_conflict());

    let missing = client
        .sandbox("odd")
        .attach(&AttachSpec::new(["true"]))
        .await
        .unwrap_err();
    assert!(matches!(missing, Error::InvalidRequest(ref m) if m.contains("Sandboxes-Exec-Id")));
}

#[test]
fn specs_serialize_every_option() {
    let exec = ExecSpec::new(["env"]).with_envs([("A", "1"), ("B", "2")]);
    assert_eq!(
        serde_json::to_value(&exec).unwrap(),
        json!({ "cmd": ["env"], "env": { "A": "1", "B": "2" } })
    );

    let attach = AttachSpec::shell("read line")
        .with_working_dir("/work")
        .with_env("A", "1")
        .with_envs([("B", "2")]);
    assert_eq!(
        serde_json::to_value(&attach).unwrap(),
        json!({ "cmd": ["sh", "-c", "read line"], "working_dir": "/work", "env": { "A": "1", "B": "2" } })
    );

    let agent = AgentSessionSpec::default()
        .with_tty(TerminalSize::new(40, 120))
        .with_env("MODE", "ci")
        .with_ssh_agent_socket("/run/ssh-agent.sock");
    assert_eq!(
        serde_json::to_value(&agent).unwrap(),
        json!({ "tty": true, "terminal_size": { "height": 40, "width": 120 }, "env": { "MODE": "ci" },
                "ssh_agent_socket_path": "/run/ssh-agent.sock" })
    );
    assert_eq!(
        serde_json::to_value(agent.without_tty()).unwrap(),
        json!({ "tty": false, "env": { "MODE": "ci" }, "ssh_agent_socket_path": "/run/ssh-agent.sock" })
    );
}

#[tokio::test]
async fn successful_commands_pass_the_check() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec",
            Reply::json(200, &json!({ "exit_code": 0, "stdout": "ok\n" })),
        )
        .start();
    let client = connect(&daemon).await;

    let output = client
        .sandbox("job-42")
        .exec(&ExecSpec::new(["true"]))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(output.stdout, "ok\n");
}

#[tokio::test]
async fn sessions_are_controlled_and_printable() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec/attach",
            Reply::upgrade("tcp", &[("sandboxes-exec-id", "e1")], frame(1, b"$ ")),
        )
        .start();
    let client = connect(&daemon).await;

    let session = client
        .sandbox("job-42")
        .attach(&AttachSpec::new(["bash"]))
        .await
        .unwrap();
    assert_eq!(session.control().id(), "e1");
    assert!(format!("{session:?}").starts_with("ExecSession { id: \"e1\""));
    let (mut input, output, _control) = session.split();
    assert_eq!(format!("{input:?}"), "ExecInput { .. }");
    assert_eq!(
        format!("{output:?}"),
        "ExecOutputStream { finished: false, .. }"
    );
    input.write_all(b"exit\n").await.unwrap();
    input.flush().await.unwrap();
    input.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_process_that_ignores_its_input_still_reports_its_outcome() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec/attach",
            Reply::upgrade("tcp", &[("sandboxes-exec-id", "e1")], frame(1, b"done\n"))
                .ignoring_input(),
        )
        .route("GET", "/sandbox/job-42/exec/e1", exited(0))
        .start();
    let client = connect(&daemon).await;

    let session = client
        .sandbox("job-42")
        .attach(&AttachSpec::new(["true"]))
        .await
        .unwrap();
    // More input than the socket buffers hold: writing it fails once the
    // process is gone.
    let output = session
        .collect(vec![b'x'; 4 << 20], EXIT_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(output.stdout, b"done\n");
    assert_eq!(output.exit_code, 0);
}

#[tokio::test]
async fn a_dropped_connection_ends_the_output() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec/attach",
            Reply::upgrade("tcp", &[("sandboxes-exec-id", "e1")], frame(1, b"$ ")).resetting(),
        )
        .start();
    let client = connect(&daemon).await;

    let session = client
        .sandbox("job-42")
        .attach(&AttachSpec::new(["bash"]))
        .await
        .unwrap();
    let (mut input, mut output, _control) = session.split();
    input.write_all(b"never read\n").await.unwrap();
    assert!(output.next().await.unwrap().is_ok());
    let end = output.next().await;
    // Linux reports input left unread by a closing peer as a reset; other
    // systems may end the stream instead.
    match end {
        Some(Err(Error::Connection(_))) => {}
        None if !cfg!(target_os = "linux") => {}
        other => panic!("the output should end with a reset: {other:?}"),
    }
    assert!(output.next().await.is_none(), "nothing follows the end");
}

#[tokio::test]
async fn waiting_for_the_exit_polls_until_the_process_ends() {
    let running = Reply::json(200, &json!({ "id": "e1", "running": true }));
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec/attach",
            Reply::upgrade("tcp", &[("sandboxes-exec-id", "e1")], Vec::new()),
        )
        .route_sequence(
            "GET",
            "/sandbox/job-42/exec/e1",
            vec![running.clone(), exited(7)],
        )
        .start();
    let client = connect(&daemon).await;

    let control = client
        .sandbox("job-42")
        .attach(&AttachSpec::new(["sleep", "1"]))
        .await
        .unwrap()
        .control();
    assert_eq!(control.wait(EXIT_TIMEOUT).await.unwrap(), 7);
    assert_eq!(
        daemon
            .requests()
            .iter()
            .filter(|r| r.path() == "/sandbox/job-42/exec/e1")
            .count(),
        2
    );
}

#[tokio::test]
async fn waiting_for_a_process_that_keeps_running_times_out() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec/attach",
            Reply::upgrade("tcp", &[("sandboxes-exec-id", "e1")], Vec::new()),
        )
        .route(
            "GET",
            "/sandbox/job-42/exec/e1",
            Reply::json(200, &json!({ "id": "e1", "running": true })),
        )
        .start();
    let client = connect(&daemon).await;

    let control = client
        .sandbox("job-42")
        .attach(&AttachSpec::new(["sleep", "infinity"]))
        .await
        .unwrap()
        .control();
    let error = control.wait(Duration::from_millis(10)).await.unwrap_err();
    assert!(matches!(
        error,
        Error::Timeout {
            operation: "execution exit",
            after
        } if after == Duration::from_millis(10)
    ));
}

#[tokio::test]
async fn upgrades_answered_without_switching_are_errors() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/exec/attach",
            Reply::json(200, &json!({ "message": "ok" })),
        )
        .route(
            "POST",
            "/sandbox/slow/exec/attach",
            Reply::upgrade("tcp", &[("sandboxes-exec-id", "e1")], Vec::new())
                .delayed(Duration::from_millis(500)),
        )
        .start();
    let client = docker_sandbox::Client::builder()
        .with_socket(daemon.socket())
        .with_timeout(Duration::from_millis(100))
        .connect()
        .await
        .unwrap();

    let error = client
        .sandbox("job-42")
        .attach(&AttachSpec::new(["true"]))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::InvalidRequest(ref m) if m.contains("instead of switching to tcp")),
        "{error}"
    );

    let error = client
        .sandbox("slow")
        .attach(&AttachSpec::new(["true"]))
        .await
        .unwrap_err();
    assert!(error.is_timeout(), "{error}");
}

#[tokio::test]
async fn agent_arguments_are_sent_with_the_session() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/sandbox/job-42/agent/session",
            Reply::upgrade("tcp", &[("sandboxes-exec-id", "a1")], Vec::new()),
        )
        .start();
    let client = connect(&daemon).await;

    let spec = AgentSessionSpec::default()
        .with_args(["-c", "echo audit-flag-args"])
        .with_working_dir("/tmp")
        .with_env("AUDIT_RUN_ENV", "run");
    let session = client.sandbox("job-42").agent_session(&spec).await.unwrap();
    drop(session);
    // The fake daemon records an upgraded request once the connection closes.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // What `run-r-env-workdir` of the `agents` suite sent, the host terminal
    // and desktop variables aside.
    let body = daemon
        .request("POST", "/sandbox/job-42/agent/session")
        .json();
    assert_eq!(body["args"], json!(["-c", "echo audit-flag-args"]));
    assert_eq!(body["working_dir"], "/tmp");
    assert_eq!(body["env"], json!({ "AUDIT_RUN_ENV": "run" }));
    assert!(
        serde_json::to_value(AgentSessionSpec::default()).unwrap()["args"].is_null(),
        "no arguments: the default command of the kit"
    );
}
