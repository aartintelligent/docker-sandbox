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

use docker_sandbox::kit::KitPackage;
use tokio::sync::Mutex as AsyncMutex;

use super::KitForge;
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

    /// The package of `forge`, reused unless `again`, else forged and kept.
    async fn take(&self, forge: &KitForge, again: bool) -> Result<KitPackage> {
        let key = forge.composition_key();
        let entry = Arc::clone(
            self.kept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.clone())
                .or_default(),
        );
        let mut kept = entry.lock().await;
        let health = forge.client().health();
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
        let mut package = forge.build().await?;
        if let Some(dir) = &saved {
            package = package.save(dir)?;
        }
        *kept = Some(package.clone());
        Ok(package)
    }
}
