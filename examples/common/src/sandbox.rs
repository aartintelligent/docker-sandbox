//! Throw-away sandboxes: created for one example, deleted whatever happens.

use std::future::Future;

use anyhow::{Result, anyhow};
use docker_sandbox::Client;
use docker_sandbox::kit::KitPackage;
use docker_sandbox::sandbox::{CreateRequest, Sandbox};

/// A sandbox name unique to this run, e.g. `example-shell-4242`.
#[must_use]
pub fn unique_name(prefix: &str) -> String {
    format!("example-{prefix}-{}", std::process::id())
}

/// Creates the sandbox `name` from `package`, runs `run` with it, then deletes
/// it, also when `run` fails or the user presses Ctrl-C.
///
/// # Errors
///
/// Returns the error of the creation, of `run`, or of the deletion.
pub async fn with_sandbox<T, F, Fut>(
    client: &Client,
    package: &KitPackage,
    name: &str,
    run: F,
) -> Result<T>
where
    F: FnOnce(Sandbox) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    with_sandbox_from(client, package, &package.request(name)?, run).await
}

/// Same as [`with_sandbox`], from a creation request obtained with
/// [`KitPackage::request`] and customised.
///
/// # Errors
///
/// Returns the error of the creation, of `run`, or of the deletion.
pub async fn with_sandbox_from<T, F, Fut>(
    client: &Client,
    package: &KitPackage,
    request: &CreateRequest,
    run: F,
) -> Result<T>
where
    F: FnOnce(Sandbox) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let name = request.name.as_str();
    eprintln!("creating sandbox {name}");
    package.create_with(client, request).await?;
    let sandbox = client.sandbox(name);
    let outcome = tokio::select! {
        outcome = run(sandbox.clone()) => outcome,
        _ = tokio::signal::ctrl_c() => Err(anyhow!("interrupted")),
    };
    eprintln!("deleting sandbox {name}");
    let deleted = sandbox.force_delete().await;
    let value = outcome?;
    deleted?;
    Ok(value)
}
