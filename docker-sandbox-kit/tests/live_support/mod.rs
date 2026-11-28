//! The plumbing of the live tests: unique sandbox names, commands run in a
//! sandbox, and a run that removes what a test created, even when it fails.
//!
//! The live tests of `docker-sandbox-kit` and of `docker-sandbox-blueprint`
//! share it, the latter including it by path rather than copying it.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]
#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test by panicking"
)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use docker_sandbox::Client;
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::image::ImageSummary;
use docker_sandbox::sandbox::Sandbox;

/// A sandbox name unique to this test run.
pub(crate) fn sandbox_name(test: &str) -> String {
    format!("docker-sandbox-{test}-{}", std::process::id())
}

/// References of every image held by the daemon.
pub(crate) async fn image_references(client: &Client) -> docker_sandbox::Result<BTreeSet<String>> {
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
pub(crate) async fn cleanup(
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
pub(crate) async fn catch_unwind<F: Future>(exercise: F) -> std::thread::Result<F::Output> {
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
pub(crate) fn resumed<T>(outcome: std::thread::Result<T>) -> T {
    outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// The standard output of `script`, run with `sh -c` in `sandbox`.
pub(crate) async fn shell(sandbox: &Sandbox, script: &str) -> docker_sandbox::Result<String> {
    Ok(sandbox
        .exec(&ExecSpec::new(["sh", "-c", script]))
        .await?
        .check()?
        .stdout)
}

/// Runs `exercise` on the sandbox `test`, in a directory of its own, then
/// removes what it created, the sandbox and the images it pulled.
pub(crate) async fn run<F, Fut, E>(test: &str, exercise: F) -> Result<(), E>
where
    F: FnOnce(Client, String, PathBuf) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: From<docker_sandbox::Error>,
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
