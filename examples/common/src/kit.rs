//! Kit packages compiled once with the `sbx` CLI, then replayed through the
//! socket alone.
//!
//! The daemon cannot resolve kit references by itself: a package captures what
//! the CLI sends once, and every later sandbox of that kit is created from it.
//! Packages are cached under `examples/target/kits/<kit>`, and compiled again
//! when the daemon runs another `sbx` version. A composition with a mixin of
//! the examples is cached as `<base>+<mixin>`, one with kit arguments as
//! `<base>@<name>=<value>,…`: delete the first after a change of the
//! mixin to compile it again.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use docker_sandbox::Client;
use docker_sandbox::kit::{CompileJob, KitCompiler, KitPackage};

/// Directory of the cached packages, in the target directory of the examples.
const CACHE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/kits");

/// The package of the kit `base` (`shell`, `claude`, `opencode`, …), compiled
/// on first use.
///
/// The first compilation runs `sbx create` once on a throw-away sandbox, which
/// can take a few minutes for an agent kit; the kit images it pulls stay on
/// the daemon as the cache of the next sandboxes.
///
/// # Errors
///
/// Fails when the `sbx` CLI cannot compile the kit.
pub async fn package(client: &Client, base: &str) -> Result<KitPackage> {
    let dir = PathBuf::from(CACHE).join(base);
    cached(client, base, CompileJob::new(base, &dir)).await
}

/// The package of the kit `base` composed with the mixin kit of the directory
/// `mixin`, compiled on first use as [`package`] is: the base kit first, then
/// the mixin.
///
/// # Errors
///
/// Fails when `mixin` is not a directory, or when the `sbx` CLI cannot compile
/// the composition.
pub async fn package_with_mixin(client: &Client, base: &str, mixin: &Path) -> Result<KitPackage> {
    let mixin = mixin
        .canonicalize()
        .with_context(|| format!("reading the mixin {}", mixin.display()))?;
    let name = mixin
        .file_name()
        .context("the mixin directory has no name")?
        .to_string_lossy()
        .into_owned();
    let label = format!("{base}+{name}");
    let dir = PathBuf::from(CACHE).join(&label);
    let job = CompileJob::new(base, &dir).with_mixin(mixin.to_string_lossy());
    cached(client, &label, job).await
}

/// The package of the kit `base` compiled with the kit arguments `args`
/// (`name=value` for every kit, `kit.name=value` for one), as
/// `sbx create --kit-arg` takes them, compiled on first use as [`package`]
/// is and cached as `<base>@<name>=<value>,…`.
///
/// # Errors
///
/// Fails when the `sbx` CLI cannot compile the kit, an argument the kit does
/// not declare or a value its pattern refuses included.
pub async fn package_with_args(
    client: &Client,
    base: &str,
    args: &[(&str, &str)],
) -> Result<KitPackage> {
    let pairs: Vec<String> = args
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    let label = format!("{base}@{}", pairs.join(","));
    let mut job = CompileJob::new(base, PathBuf::from(CACHE).join(&label));
    for (name, value) in args {
        job = job.with_arg(*name, *value);
    }
    cached(client, &label, job).await
}

/// The package of `job`, from the cache when it was compiled for the `sbx`
/// version the daemon runs, else compiled with the CLI.
async fn cached(client: &Client, label: &str, job: CompileJob) -> Result<KitPackage> {
    if let Ok(package) = KitPackage::open(&job.output)
        && package.compiled_with() == Some(client.health().version.as_str())
    {
        return Ok(package);
    }
    eprintln!("compiling the {label} kit with the sbx CLI (first run only, it can take a while)");
    let work = std::env::temp_dir().join(format!("example-kc-{}", std::process::id()));
    let compiled = KitCompiler::new("sbx", client.clone(), &work)
        .compile(&job)
        .await;
    let _ = std::fs::remove_dir_all(&work);
    compiled.with_context(|| format!("compiling the {label} kit"))
}
