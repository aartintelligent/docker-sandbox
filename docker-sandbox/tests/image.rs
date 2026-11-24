//! Image operations against a fake daemon: inspection, existence, export and
//! removal, and the decoding of the progress lines of a pull.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

mod support;

use docker_sandbox::Error;
use docker_sandbox::image::ImageSummary;
use serde_json::json;
use support::{FakeDaemon, Reply, connect};

#[tokio::test]
async fn images_are_inspected_by_reference() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/docker/images/inspect",
            Reply::json(
                200,
                &json!({ "id": "sha256:6bde", "agent": "shell", "cmd": ["bash", "-l"], "user": "1000" }),
            ),
        )
        .route(
            "GET",
            "/docker/images/local-metadata",
            Reply::json(200, &json!({ "local_snapshot": true })),
        )
        .start();
    let client = connect(&daemon).await;
    let images = client.images();

    let inspect = images.inspect("docker.io/sbx/shell:latest").await.unwrap();
    assert_eq!(inspect.agent.as_deref(), Some("shell"));
    assert_eq!(inspect.cmd, ["bash", "-l"]);
    assert_eq!(
        daemon.request("GET", "/docker/images/inspect").query()["name"],
        "docker.io/sbx/shell:latest"
    );

    let metadata = images.local_metadata("job-42-snapshot:1").await.unwrap();
    assert!(metadata.local_snapshot);
    assert_eq!(
        daemon
            .request("GET", "/docker/images/local-metadata")
            .query()["name"],
        "job-42-snapshot:1"
    );
}

#[tokio::test]
async fn a_missing_image_is_not_an_error_for_exists() {
    for (status, expected) in [(200, Some(true)), (404, Some(false)), (500, None)] {
        let body = if status == 200 {
            json!({ "id": "sha256:6bde" })
        } else {
            json!({ "message": "image inspection failed" })
        };
        let daemon = FakeDaemon::builder()
            .route("GET", "/docker/images/inspect", Reply::json(status, &body))
            .start();
        let client = connect(&daemon).await;

        let exists = client.images().exists("docker.io/sbx/shell:latest").await;
        assert_eq!(exists.ok(), expected, "inspection answered {status}");
    }
}

#[tokio::test]
async fn images_are_exported_and_removed() {
    let daemon = FakeDaemon::builder()
        .route(
            "GET",
            "/docker/images/export",
            Reply::raw(200, "application/x-tar", b"tar bytes".as_slice()),
        )
        .route("DELETE", "/docker/images/remove", Reply::empty(200))
        .start();
    let client = connect(&daemon).await;
    let images = client.images();

    let archive = images.export("sbx-kit-src:spec-abc").await.unwrap();
    assert_eq!(archive.as_ref(), b"tar bytes");
    let export = daemon.request("GET", "/docker/images/export");
    assert_eq!(export.query()["name"], "sbx-kit-src:spec-abc");
    assert_eq!(export.headers["accept"], "application/x-tar");

    images.remove("sbx-kit-src:spec-abc").await.unwrap();
    assert_eq!(
        daemon.request("DELETE", "/docker/images/remove").query()["name"],
        "sbx-kit-src:spec-abc"
    );
}

#[test]
fn references_prefer_the_tag_then_the_digest_then_the_identifier() {
    let reference = |image: serde_json::Value| {
        serde_json::from_value::<ImageSummary>(image)
            .unwrap()
            .reference()
    };

    assert_eq!(
        reference(
            json!({ "repository": "docker.io/sbx/shell", "tag": "latest", "id": "sha256:1", "digest": "sha256:2" })
        ),
        "docker.io/sbx/shell:latest"
    );
    assert_eq!(
        reference(
            json!({ "repository": "docker.io/sbx/shell", "tag": "", "id": "sha256:1", "digest": "sha256:2" })
        ),
        "docker.io/sbx/shell@sha256:2"
    );
    assert_eq!(
        reference(json!({ "repository": "sbx-kit-src", "id": "sha256:1" })),
        "sbx-kit-src@sha256:1"
    );
}

#[tokio::test]
async fn pull_progress_skips_blank_lines_and_rejects_malformed_ones() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/docker/images/create",
            Reply::raw(
                200,
                "application/x-ndjson",
                "\n{\"status\":\"Pulling\"}\n\nnot json\n",
            ),
        )
        .start();
    let client = connect(&daemon).await;

    let mut stream = client.images().pull_stream("sbx/shell").await.unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.status.as_deref(), Some("Pulling"));
    assert!(matches!(
        stream.next().await.unwrap(),
        Err(Error::Decode {
            operation: "image pull",
            ..
        })
    ));
}

#[tokio::test]
async fn pull_progress_of_the_wrong_shape_is_a_decoding_error() {
    let daemon = FakeDaemon::builder()
        .route(
            "POST",
            "/docker/images/create",
            Reply::ndjson(&[json!({ "status": 42 })]),
        )
        .start();
    let client = connect(&daemon).await;

    let error = client.images().pull("sbx/shell").await.unwrap_err();
    assert!(matches!(
        error,
        Error::Decode {
            operation: "image pull",
            ..
        }
    ));
}
