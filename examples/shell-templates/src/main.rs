//! A shell sandbox saved as an image, a new sandbox created from that image
//! with what the first one held, and the image exported to a file and loaded
//! back, as `sbx template save`, `sbx create -t` and `sbx template load` do.
//!
//! The first sandbox gets a file, is stopped and saved (`Sandbox::save`); the
//! second starts from the saved image (`CreateRequest::with_template`, with
//! `PullPolicy::Never`: the image is on this host only), its request stored as
//! JSON and replayed (`CreateRequest::from_template`), and finds the file.
//! The image is then exported to `examples/target/templates/`, written as the
//! daemon streams it (`Images::export_stream`), removed, and loaded back from
//! the file (`Images::load_file`), the way a template moves
//! to another host. The image and the file are removed at the end, also on
//! an error.
//!
//! ```sh
//! just example shell-templates
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use docker_sandbox::Client;
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::kit::KitPackage;
use docker_sandbox::sandbox::{CreateRequest, PullPolicy};
use example_common::{connect, kit, sandbox};
use tokio::io::AsyncWriteExt;

/// Directory of the exported images.
const EXPORTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/templates");

/// The file the first sandbox holds when it is saved.
const SAVED_FILE: &str = "/home/agent/saved.txt";

/// What that file says.
const SAVED_TEXT: &str = "written before the save\n";

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let tag = format!("example-snapshot-{}:v1", std::process::id());
    let archive =
        PathBuf::from(EXPORTS).join(format!("example-snapshot-{}.tar", std::process::id()));

    let outcome = run(&client, &package, &tag, &archive).await;
    // Whatever happened, the image and its file go.
    let images = client.images();
    if images.exists(&tag).await? {
        images.remove(&tag).await?;
        println!("removed the image {tag}");
    }
    let _ = std::fs::remove_file(&archive);
    outcome
}

/// Saves a sandbox as `tag`, creates another from it, then exports the image
/// to `archive` and loads it back.
async fn run(client: &Client, package: &KitPackage, tag: &str, archive: &Path) -> Result<()> {
    let first = sandbox::unique_name("template-source");
    let saved = sandbox::with_sandbox(client, package, &first, |sandbox| async move {
        let running = sandbox.ensure_running().await?;
        sandbox.write_file(SAVED_FILE, SAVED_TEXT).await?;
        drop(running);
        sandbox.stop().await?;
        Ok(sandbox.save(tag).await?)
    })
    .await?;
    println!("saved {first} as {} ({})", saved.tag, saved.image_id);

    // A creation request is plain data: an application can store it, as JSON
    // in its database for instance, and replay it under another name.
    let stored = serde_json::to_value(
        package
            .request("{{name}}")?
            .with_template(tag)
            .with_pull_policy(PullPolicy::Never),
    )?;
    let second = sandbox::unique_name("template-copy");
    let request = CreateRequest::from_template(stored, &second)?;
    sandbox::with_sandbox_from(client, package, &request, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        let text = sandbox
            .exec(&ExecSpec::new(["cat", SAVED_FILE]))
            .await?
            .check()?
            .stdout;
        ensure!(text == SAVED_TEXT, "the new sandbox holds {text:?}");
        println!(
            "{} starts from the image and holds {SAVED_FILE}",
            sandbox.name()
        );
        Ok(())
    })
    .await?;

    // The way a template moves to another host: a file, loaded there. The
    // archive is written as the daemon sends it, never held whole in memory.
    let images = client.images();
    std::fs::create_dir_all(EXPORTS)?;
    let mut file = tokio::fs::File::create(archive)
        .await
        .with_context(|| format!("creating {}", archive.display()))?;
    let mut export = images.export_stream(tag).await?;
    let mut size = 0;
    while let Some(chunk) = export.next().await {
        let chunk = chunk?;
        size += chunk.len();
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    println!(
        "exported to a file of {} MiB, chunk by chunk",
        size / (1 << 20)
    );
    images.remove(tag).await?;
    ensure!(!images.exists(tag).await?, "the image is still there");
    println!("image removed from the daemon");
    images.load_file(archive).await?;
    ensure!(images.exists(tag).await?, "the image did not come back");
    println!("loaded back from the file: {tag} is there again");
    Ok(())
}
