//! Kit packages kept by composition, forged once and reused while they suit
//! the daemon.
//!
//! Forging a kit costs about a second even when its images are on the daemon:
//! the image of the base kit is exported again to read its descriptor. A
//! program that creates many sandboxes of one composition keeps its package
//! in a [`KitCache`], in memory, and in a directory when it is given one, so
//! that the next process takes the package from there. A package is reused
//! while it suits the daemon (`KitPackage::suits`) and forged again otherwise,
//! or when [`KitCache::refresh`] asks: the cache never asks the registry
//! whether a published kit moved.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use docker_sandbox::daemon::Health;
use docker_sandbox::kit::KitPackage;
use tokio::sync::Mutex as AsyncMutex;

use super::KitForge;
#[cfg(feature = "compiler")]
use crate::compiler::{CompileJob, KitCompiler};
use crate::error::Result;

/// The packages of a cache, by composition, each behind a lock of its own so
/// that a composition is forged once at a time.
type Kept = HashMap<String, Arc<AsyncMutex<Option<KitPackage>>>>;

/// Kit packages kept by composition (feature `forge`): the package of a
/// [`KitForge`], forged once and reused while it suits the daemon.
///
/// A composition is its base kit, its arguments and its mixins, the content
/// of their descriptors included: a mixin changed makes another composition.
/// Clones share their packages.
///
/// ```no_run
/// use docker_sandbox::Client;
/// use docker_sandbox_kit::{Agent, KitCache, KitForge};
///
/// # async fn run() -> docker_sandbox_kit::Result<()> {
/// let client = Client::connect().await?;
/// let cache = KitCache::new().with_dir("/var/cache/kits");
/// let forge = KitForge::agent(client.clone(), Agent::Claude).with_arg("model", "claude-haiku-5-5");
/// for job in ["job-1", "job-2"] {
///     // Forged for the first job, then taken from the cache.
///     cache.package(&forge).await?.create(&client, job).await?;
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct KitCache {
    /// Directory the packages are kept in too, one directory per composition.
    dir: Option<PathBuf>,
    /// The packages kept in memory.
    kept: Arc<Mutex<Kept>>,
}

impl KitCache {
    /// A cache in memory, empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Keeps the packages in the directory `dir` too, one directory per
    /// composition named by its key: a package found there is reused while
    /// it suits the daemon, by this cache and by the next process.
    #[must_use]
    pub fn with_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    /// The directory the packages are kept in, if the cache has one.
    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// The package of the composition of `forge`: the one kept, in memory
    /// or in the directory, while it suits the daemon of `forge`, else one
    /// forged ([`KitForge::build`]), then kept.
    ///
    /// # Errors
    ///
    /// As [`KitForge::build`] when the package is forged, and
    /// [`Error::Client`](crate::Error::Client)
    /// ([`docker_sandbox::Error::Package`]) when it cannot be saved to the
    /// directory.
    pub async fn package(&self, forge: &KitForge) -> Result<KitPackage> {
        self.take(forge, false).await
    }

    /// The package of the composition of `forge`, forged again in place of
    /// the one kept: for a published kit that moved since, which the cache
    /// does not see by itself.
    ///
    /// # Errors
    ///
    /// As [`KitCache::package`].
    pub async fn refresh(&self, forge: &KitForge) -> Result<KitPackage> {
        self.take(forge, true).await
    }

    /// The package of `job`, compiled by `compiler` with the `sbx` CLI
    /// (feature `compiler`): the one kept, in memory or in the directory,
    /// while it suits the daemon (compiled by the `sbx` version it runs),
    /// else one compiled, then kept. A composition is its base kit, its
    /// arguments and its mixins, the text of the `spec.yaml` of a mixin given
    /// as a directory and the descriptor of one described in code included:
    /// a mixin changed is compiled again. With a directory, the package is
    /// written under it, in place of the `output` of the job.
    ///
    /// ```no_run
    /// # async fn run(client: docker_sandbox::Client) -> docker_sandbox_kit::Result<()> {
    /// use docker_sandbox_kit::{CompileJob, KitCache, KitCompiler};
    ///
    /// let cache = KitCache::new().with_dir("/var/cache/kits");
    /// let compiler = KitCompiler::new("sbx", client.clone(), "/tmp/kc");
    /// let job = CompileJob::new("git+https://github.com/acme/kit.git", "/tmp/unused");
    /// // Compiled once, then taken from the cache while the daemon runs the
    /// // same sbx version.
    /// let package = cache.compiled(&compiler, &job).await?;
    /// # let _ = package;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`KitCompiler::compile`].
    #[cfg(feature = "compiler")]
    #[cfg_attr(docsrs, doc(cfg(feature = "compiler")))]
    pub async fn compiled(&self, compiler: &KitCompiler, job: &CompileJob) -> Result<KitPackage> {
        self.take_compiled(compiler, job, false).await
    }

    /// The package of `job`, compiled again in place of the one kept: for a
    /// kit of a Git repository or a registry that moved since, which the
    /// cache does not see by itself.
    ///
    /// # Errors
    ///
    /// As [`KitCache::compiled`].
    #[cfg(feature = "compiler")]
    #[cfg_attr(docsrs, doc(cfg(feature = "compiler")))]
    pub async fn recompile(&self, compiler: &KitCompiler, job: &CompileJob) -> Result<KitPackage> {
        self.take_compiled(compiler, job, true).await
    }

    /// The package of `forge`, reused unless `again`, else forged and kept.
    async fn take(&self, forge: &KitForge, again: bool) -> Result<KitPackage> {
        let key = forge.composition_key();
        self.keep(key, forge.client().health(), again, async |saved| {
            let package = forge.build().await?;
            match saved {
                Some(dir) => Ok(package.save(dir)?),
                None => Ok(package),
            }
        })
        .await
    }

    /// The package of `job`, reused unless `again`, else compiled and kept.
    #[cfg(feature = "compiler")]
    async fn take_compiled(
        &self,
        compiler: &KitCompiler,
        job: &CompileJob,
        again: bool,
    ) -> Result<KitPackage> {
        let key = compiled_key(job);
        self.keep(key, compiler.client().health(), again, async |saved| {
            let mut job = job.clone();
            if let Some(dir) = saved {
                job.output = dir.to_owned();
            }
            compiler.compile(&job).await
        })
        .await
    }

    /// The package of the composition `key`: the one kept, in memory or in
    /// the directory, while it suits the daemon of `health`, unless `again`;
    /// else the one `make` makes, given the directory it is to be kept in,
    /// then kept.
    async fn keep(
        &self,
        key: String,
        health: &Health,
        again: bool,
        make: impl AsyncFnOnce(Option<&Path>) -> Result<KitPackage>,
    ) -> Result<KitPackage> {
        let entry = Arc::clone(
            self.kept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.clone())
                .or_default(),
        );
        let mut kept = entry.lock().await;
        let saved = self.dir.as_ref().map(|dir| dir.join(&key));
        if !again {
            if let Some(package) = kept.as_ref().filter(|package| package.suits(health)) {
                return Ok(package.clone());
            }
            if let Some(package) = saved
                .as_ref()
                .and_then(|dir| KitPackage::open(dir).ok())
                .filter(|package| package.suits(health))
            {
                *kept = Some(package.clone());
                return Ok(package);
            }
        }
        let package = make(saved.as_deref()).await?;
        *kept = Some(package.clone());
        Ok(package)
    }
}

/// The key of the composition of `job`, kept apart from those of the forge:
/// the first 16 hexadecimal digits of a SHA-256 digest of its base kit, its
/// arguments and its mixins, with the text of the `spec.yaml` of a mixin
/// given as a directory, and the descriptor of a mixin described in code.
#[cfg(feature = "compiler")]
fn compiled_key(job: &CompileJob) -> String {
    let mixins: Vec<serde_json::Value> = job
        .mixins
        .iter()
        .map(|mixin| {
            let spec = std::fs::read_to_string(Path::new(mixin).join("spec.yaml")).ok();
            serde_json::json!({ "reference": mixin, "spec": spec })
        })
        .collect();
    let described: Vec<serde_json::Value> = job
        .mixin_specs
        .iter()
        .map(|mixin| serde_json::json!({ "reference": mixin.reference(), "descriptor": mixin.descriptor().to_json() }))
        .collect();
    let composition = serde_json::json!({
        "compiled": { "base": job.base, "args": job.args, "mixins": mixins, "described": described }
    });
    let digest = super::oci::digest(composition.to_string().as_bytes());
    digest["sha256:".len().."sha256:".len() + 16].to_owned()
}
