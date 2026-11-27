//! `kit-compile`: compiles local kits into a package replayable through the
//! socket alone.
//!
//! The compilation itself is done by [`docker_sandbox_kit::KitCompiler`]: it runs
//! `sbx create` once on a throw-away sandbox behind a capturing proxy and writes
//! the exact bytes the CLI sent to the daemon:
//!
//! ```text
//! <out>/kit.json            metadata (sbx version, composition, images)
//! <out>/create-body.json    creation request template, "name": "{{name}}"
//! <out>/images/NN.oci.tar   kit source images, loaded before creation
//! ```
//!
//! On top of it, this command leaves the daemon as it found it (images created
//! during the compilation are removed) and, unless disabled, verifies the
//! package by replaying it through the socket only with [`KitPackage`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use docker_sandbox::Client;
use docker_sandbox::image::ImageSummary;
use docker_sandbox::kit::KitPackage;
use docker_sandbox_kit::{CompileJob, KitCompiler};

/// Options of the `kit-compile` command.
pub(crate) struct KitcOpts<'a> {
    /// `sbx` executable.
    pub sbx: &'a str,
    /// Daemon socket.
    pub socket: &'a Path,
    /// Base kit: a built-in agent name or a kit reference.
    pub base: &'a str,
    /// Mixin directories, composed with the base kit in order.
    pub kits: &'a [PathBuf],
    /// Kit arguments, as `name=value` or `kit.name=value`.
    pub kit_args: &'a [String],
    /// Output directory of the package.
    pub out: &'a Path,
    /// Whether to replay the package through the socket after compiling it.
    pub verify: bool,
    /// Maximum duration of the compilation.
    pub timeout: Duration,
}

/// Compiles the kits, cleans the daemon up and optionally verifies the package.
pub(crate) async fn run(opts: &KitcOpts<'_>) -> Result<()> {
    let client = Client::builder()
        .with_socket(opts.socket)
        .connect()
        .await
        .context("connecting to sandboxd")?;
    let health = client.health();
    eprintln!(
        "[kit-compile] sbx {} (API {})",
        health.version, health.api_version
    );

    let mixins = opts
        .kits
        .iter()
        .map(|kit| {
            kit.canonicalize()
                .with_context(|| format!("kit directory not found: {}", kit.display()))
                .map(|p| p.display().to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    let job = CompileJob {
        mixins,
        args: parse_kit_args(opts.kit_args)?,
        ..CompileJob::new(opts.base, opts.out)
    };

    let images_before = image_references(&client).await?;
    let work_dir =
        std::env::temp_dir().join(format!("docker-sandbox-audit-kc-{}", std::process::id()));
    let compiler = KitCompiler::new(opts.sbx, client.clone(), &work_dir).with_timeout(opts.timeout);
    let outcome = compiler.compile(&job).await;
    let _ = std::fs::remove_dir_all(&work_dir);
    let removed = remove_new_images(&client, &images_before).await?;
    let package = outcome.context("compiling the kits")?;
    eprintln!(
        "[kit-compile] package written to {} ({} kit(s), {} image(s)); {removed} image(s) removed from sandboxd",
        opts.out.display(),
        package.kit_refs().len(),
        package.images().len()
    );

    if opts.verify {
        verify(&client, &package).await?;
    }
    Ok(())
}

/// Replays `package` through the socket alone on a daemon that does not hold
/// its images, then removes everything it created.
async fn verify(client: &Client, package: &KitPackage) -> Result<()> {
    let name = format!("docker-sandbox-audit-verify-{}", std::process::id());
    let images_before = image_references(client).await?;

    let created = package.create(client, name.as_str()).await;
    let sandbox = client.sandbox(&name);
    if sandbox.exists().await? {
        sandbox
            .force_delete()
            .await
            .context("deleting the verification sandbox")?;
    }
    let removed = remove_new_images(client, &images_before).await?;
    let created = created.context("the package could not be replayed through the socket")?;

    eprintln!(
        "[kit-compile] verified: sandbox {} created through the socket alone (agent {}), then deleted; {removed} image(s) removed",
        created.name, created.agent
    );
    Ok(())
}

/// Parses `name=value` kit arguments.
pub(crate) fn parse_kit_args(args: &[String]) -> Result<BTreeMap<String, String>> {
    args.iter()
        .map(|arg| match arg.split_once('=') {
            Some((name, value)) if !name.is_empty() => Ok((name.to_owned(), value.to_owned())),
            _ => bail!("invalid kit argument {arg:?}: expected name=value"),
        })
        .collect()
}

async fn image_references(client: &Client) -> Result<BTreeSet<String>> {
    Ok(client
        .images()
        .list()
        .await
        .context("listing images")?
        .iter()
        .map(ImageSummary::reference)
        .collect())
}

/// Removes the images that appeared since `before`, derived tags first.
/// Returns how many were removed.
async fn remove_new_images(client: &Client, before: &BTreeSet<String>) -> Result<usize> {
    let mut created: Vec<String> = image_references(client)
        .await?
        .difference(before)
        .cloned()
        .collect();
    created.sort_by_key(|reference| std::cmp::Reverse(reference.len()));
    for reference in &created {
        client
            .images()
            .remove(reference)
            .await
            .with_context(|| format!("removing image {reference}"))?;
    }
    Ok(created.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kit_arguments_are_parsed() {
        let args = parse_kit_args(&[
            "host=db.internal".into(),
            "kit.port=5432".into(),
            "empty=".into(),
        ])
        .unwrap();
        assert_eq!(args["host"], "db.internal");
        assert_eq!(args["kit.port"], "5432");
        assert_eq!(args["empty"], "");
        assert!(parse_kit_args(&["novalue".into()]).is_err());
        assert!(parse_kit_args(&["=value".into()]).is_err());
    }
}
