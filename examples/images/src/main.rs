//! The images the daemon holds, as a program manages them: the list, the
//! details of one, a pull followed as it happens, the same pull waited for,
//! and what an unknown image gives.
//!
//! The image pulled is the one of the `shell` kit, `docker.io/sbx/shell`, as
//! `sbx` pulls it. When its `latest` tag was not on the daemon before, the
//! example removes the tag at the end, so that the daemon holds what it held.
//! Saving a sandbox as an image, exporting and loading it are in
//! `shell-templates`.
//!
//! ```sh
//! just example images
//! ```

use anyhow::{Context, Result, ensure};
use docker_sandbox::image::{ImageSummary, Images};
use example_common::connect;

/// The image pulled by the example, as `sbx` pulls the `shell` kit.
const KIT_IMAGE: &str = "docker.io/sbx/shell:latest";

/// An image that does not exist.
const UNKNOWN: &str = "example-no-such-image:latest";

/// Images listed, out of all the daemon holds.
const LISTED: usize = 5;

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let images = client.images();
    list(&images).await?;

    let had = images.exists(KIT_IMAGE).await?;
    println!(
        "{KIT_IMAGE}: {}",
        if had { "already held" } else { "not held yet" }
    );
    let outcome = pull_and_inspect(&images).await;
    // Leave the daemon as it was, whatever happened.
    if !had && images.exists(KIT_IMAGE).await? {
        images.remove(KIT_IMAGE).await?;
        println!("removed the {KIT_IMAGE} tag the example added");
    }
    outcome
}

/// Lists the images of the daemon, the largest first.
async fn list(images: &Images) -> Result<()> {
    let mut all: Vec<ImageSummary> = images.list().await?;
    all.sort_by_key(|image| std::cmp::Reverse(image.size.unwrap_or_default()));
    let total: u64 = all.iter().filter_map(|image| image.size).sum();
    println!("{} images, {} MiB in all", all.len(), total / (1 << 20));
    for image in all.iter().take(LISTED) {
        println!(
            "  {:<70} {:>6} MiB{}",
            image.reference(),
            image.size.unwrap_or_default() / (1 << 20),
            image
                .agent
                .as_deref()
                .map(|agent| format!(", agent {agent}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// Pulls the kit image while following its progress, pulls it again waiting
/// for the outcome, then reads its details and those of an unknown image.
async fn pull_and_inspect(images: &Images) -> Result<()> {
    let mut progress = images.pull_stream(KIT_IMAGE).await?;
    let (mut lines, mut last) = (0, String::new());
    while let Some(line) = progress.next().await {
        let line = line?;
        if let Some(error) = line.error {
            anyhow::bail!("pull failed: {error}");
        }
        lines += 1;
        // Layers report the same status many times: print each change once.
        let status = line.status.unwrap_or_default();
        if status != last {
            println!("  pull: {status}");
            last = status;
        }
    }
    println!("pulled {KIT_IMAGE}: {lines} progress lines");
    images.pull(KIT_IMAGE).await?;
    println!("pulled again, waiting for the outcome: nothing to download");

    let inspect = images.inspect(KIT_IMAGE).await?;
    let metadata = images.local_metadata(KIT_IMAGE).await?;
    println!(
        "{KIT_IMAGE}: id {}, agent {}, user {}, entrypoint {:?}, a local snapshot: {}",
        inspect.id,
        inspect.agent.as_deref().unwrap_or("none"),
        inspect.user.as_deref().unwrap_or("default"),
        inspect.entrypoint,
        metadata.local_snapshot
    );

    ensure!(!images.exists(UNKNOWN).await?, "{UNKNOWN} exists");
    let error = images
        .export(UNKNOWN)
        .await
        .err()
        .context("an unknown image was exported")?;
    ensure!(error.is_not_found(), "unexpected error: {error}");
    let metadata = images.local_metadata(UNKNOWN).await?;
    println!(
        "{UNKNOWN}: does not exist, its export is not found, its local metadata the defaults (a local snapshot: {})",
        metadata.local_snapshot
    );
    Ok(())
}
