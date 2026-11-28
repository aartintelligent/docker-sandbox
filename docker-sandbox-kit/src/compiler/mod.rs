//! Compiling kit compositions into kit packages (feature `compiler`).
//!
//! `sandboxd` cannot resolve kits: the `sbx` CLI assembles the `kit_artifacts`
//! of a creation request and loads local kit sources as images. Compiling a
//! composition means running `sbx create` once on a throw-away sandbox behind a
//! capturing proxy and keeping the exact bytes the CLI sent, as a
//! [`KitPackage`] that replays through the socket alone:
//!
//! ```text
//! <output>/kit.json            metadata (sbx version, composition, images)
//! <output>/create-body.json    creation request template, "name": "{{name}}"
//! <output>/images/NN.oci.tar   kit source images
//! ```
//!
//! The CLI must run as the user owning the daemon, not as root: as root it
//! ignores `DOCKER_SANDBOXES_API`, through which it is pointed at the proxy.

mod proxy;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;

use self::proxy::{CaptureProxy, CaptureRules, CapturedRequest};
use crate::error::{Error, Result};
use docker_sandbox::kit::{KitPackage, PackageImage};
use docker_sandbox::{Client, SOCKET_ENV};

/// Requests of the CLI kept in the package: the creation request and the kit
/// source images it loads.
const CAPTURE_RULES: CaptureRules = &[("POST", "/sandbox"), ("POST", "/docker/images/load")];

/// Default maximum duration of one compilation.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Lines of the CLI's standard error kept in a compilation error.
const STDERR_TAIL: usize = 5;

/// A composition to compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileJob {
    /// Base kit, as accepted by `sbx create`: a built-in agent name, an OCI
    /// reference or a directory.
    pub base: String,
    /// Mixins, as accepted by `sbx create --kit`.
    pub mixins: Vec<String>,
    /// Mixins described in code (feature `forge`), composed after
    /// [`CompileJob::mixins`]: each is written into a directory of the work
    /// directory of the compiler, given to the CLI, and removed with it.
    #[cfg(feature = "forge")]
    #[cfg_attr(docsrs, doc(cfg(feature = "forge")))]
    pub mixin_specs: Vec<crate::Mixin>,
    /// Kit arguments (`name=value` for all kits, `kit.name=value` for one).
    pub args: BTreeMap<String, String>,
    /// Directory to write the package to. Replaced when it exists.
    pub output: PathBuf,
}

impl CompileJob {
    /// Compiles `base` alone into `output`.
    ///
    /// ```
    /// use docker_sandbox_kit::CompileJob;
    ///
    /// let job = CompileJob::new("shell", "/var/lib/kits/shell-session")
    ///     .with_mixin("docker.io/sbx/session:latest")
    ///     .with_arg("region", "eu");
    /// assert_eq!(job.mixins, ["docker.io/sbx/session:latest"]);
    /// ```
    pub fn new(base: impl Into<String>, output: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            mixins: Vec::new(),
            #[cfg(feature = "forge")]
            mixin_specs: Vec::new(),
            args: BTreeMap::new(),
            output: output.into(),
        }
    }

    /// Appends a mixin, as accepted by `sbx create --kit`.
    #[must_use]
    pub fn with_mixin(mut self, mixin: impl Into<String>) -> Self {
        self.mixins.push(mixin.into());
        self
    }

    /// Appends a mixin described in code (feature `forge`), composed after
    /// the mixins given by reference. The compiler writes it into a directory
    /// of its work directory named after the mixin, gives that directory to
    /// the CLI, and removes it once the package is written: no directory of
    /// the program is needed.
    ///
    /// ```
    /// use docker_sandbox_kit::{CompileJob, KitDescriptor, LifecycleStep, Mixin};
    ///
    /// let tools = KitDescriptor::mixin("Acme tools")
    ///     .with_install(LifecycleStep::new("pip install acme-cli").as_user("agent"));
    /// let job = CompileJob::new("git+https://github.com/acme/kit.git", "/var/lib/kits/acme")
    ///     .with_mixin_spec(Mixin::new("/kits/acme-tools", tools)?);
    /// assert_eq!(job.mixin_specs[0].name(), "acme-tools");
    /// # Ok::<(), docker_sandbox_kit::Error>(())
    /// ```
    #[cfg(feature = "forge")]
    #[cfg_attr(docsrs, doc(cfg(feature = "forge")))]
    #[must_use]
    pub fn with_mixin_spec(mut self, mixin: crate::Mixin) -> Self {
        self.mixin_specs.push(mixin);
        self
    }

    /// Sets a kit argument: `name` applies to every kit, `kit.name` to one.
    #[must_use]
    pub fn with_arg(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.args.insert(name.into(), value.into());
        self
    }
}

/// Compiles kit compositions with the `sbx` CLI.
///
/// ```no_run
/// use docker_sandbox::Client;
/// use docker_sandbox_kit::{CompileJob, KitCompiler};
///
/// # async fn run() -> docker_sandbox_kit::Result<()> {
/// let client = Client::connect().await?;
/// let compiler = KitCompiler::new("sbx", client.clone(), "/tmp/kc");
/// let package = compiler
///     .compile(&CompileJob::new("shell", "/var/lib/kits/my-mixin").with_mixin("./my-mixin"))
///     .await?;
/// package.create(&client, "job-42").await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct KitCompiler {
    /// `sbx` executable.
    sbx: PathBuf,
    /// Client of the daemon the compilation runs against.
    client: Client,
    /// Directory of the temporary proxy sockets.
    work_dir: PathBuf,
    /// Maximum duration of one compilation.
    timeout: Duration,
}

impl KitCompiler {
    /// Compiles with the `sbx` executable at `sbx` (looked up in `PATH` when
    /// relative), against the daemon of `client`, using `work_dir` for
    /// temporary sockets.
    ///
    /// `work_dir` must be short: Unix socket paths are limited to about 100
    /// bytes.
    pub fn new(sbx: impl Into<PathBuf>, client: Client, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            sbx: sbx.into(),
            client,
            work_dir: work_dir.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Sets the maximum duration of one compilation (default: 10 minutes).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Compiles `job` and saves the package to `job.output`, as
    /// [`KitPackage::save`] saves any package, replacing one already there.
    ///
    /// The throw-away sandbox is removed whatever happens. The kit images the
    /// CLI built are kept: they are the cache that sandboxes of this
    /// composition reuse.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compile`] when the CLI cannot run, rejects the
    /// composition or sends no creation request, [`Error::Client`] ([`docker_sandbox::Error::Timeout`]) when it
    /// exceeds the timeout, and [`Error::Client`] ([`docker_sandbox::Error::Package`]) when the package cannot be
    /// written.
    pub async fn compile(&self, job: &CompileJob) -> Result<KitPackage> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let sandbox = format!("sbx-kit-compile-{}-{n}", std::process::id());

        std::fs::create_dir_all(&self.work_dir).map_err(|e| package_error(&self.work_dir, e))?;
        let socket = self.work_dir.join(format!("kc{n}.sock"));
        let proxy = CaptureProxy::start(
            socket.clone(),
            self.client.socket_path().to_owned(),
            CAPTURE_RULES,
        )
        .map_err(|e| package_error(&socket, e))?;
        #[cfg(feature = "forge")]
        let written = write_mixins(&self.work_dir, job, n)?;
        // Without the forge, a job describes no mixin in code.
        #[cfg(not(feature = "forge"))]
        let written = Vec::new();
        let ran = self.run_cli(job, &written, &sandbox, proxy.socket()).await;
        let captured = proxy.finish();
        for dir in &written {
            let _ = std::fs::remove_dir_all(dir.parent().unwrap_or(dir));
        }

        let throw_away = self.client.sandbox(&sandbox);
        if let Ok(true) = throw_away.exists().await
            && let Err(e) = throw_away.force_delete().await
        {
            tracing::warn!(sandbox, error = %e, "cannot delete the kit compilation sandbox");
        }
        ran?;

        let health = self.client.health();
        let package = build_package(
            job,
            &captured,
            &json!({ "sbx": health.version, "api_version": health.api_version }),
        )?;
        package.save(&job.output).map_err(Error::from)
    }

    /// Runs `sbx create` for `job` and the mixins written into `written`, on
    /// the throw-away `sandbox`, pointed at the `proxy` socket.
    async fn run_cli(
        &self,
        job: &CompileJob,
        written: &[PathBuf],
        sandbox: &str,
        proxy: &Path,
    ) -> Result<()> {
        let mut command = Command::new(&self.sbx);
        command.args(["--no-input", "create", "--name", sandbox, "--no-progress"]);
        for mixin in &job.mixins {
            command.args(["--kit", mixin]);
        }
        for dir in written {
            command.arg("--kit").arg(dir);
        }
        for (key, value) in &job.args {
            command.arg("--kit-arg").arg(format!("{key}={value}"));
        }
        command
            .arg(&job.base)
            .env(SOCKET_ENV, proxy)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .map_err(|_| docker_sandbox::Error::Timeout {
                operation: "kit compilation",
                after: self.timeout,
            })?
            .map_err(|e| Error::Compile(format!("cannot run {}: {e}", self.sbx.display())))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        let tail = lines[lines.len().saturating_sub(STDERR_TAIL)..].join("\n");
        Err(Error::Compile(format!(
            "sbx could not build the kit composition: {tail}"
        )))
    }
}

/// Writes the mixins described in code of `job`, for the compilation `n`,
/// each into a directory of `work_dir` named after it, and returns those
/// directories.
#[cfg(feature = "forge")]
fn write_mixins(work_dir: &Path, job: &CompileJob, n: u64) -> Result<Vec<PathBuf>> {
    job.mixin_specs
        .iter()
        .enumerate()
        .map(|(index, mixin)| {
            let dir = work_dir
                .join(format!("kc{n}-mixin-{index}"))
                .join(mixin.name());
            mixin.save(&dir).map(|_| dir)
        })
        .collect()
}

/// The package of `job`, built in memory from the captured requests: it is
/// saved as any package is ([`KitPackage::save`]).
fn build_package(
    job: &CompileJob,
    captured: &[CapturedRequest],
    compiled_with: &Value,
) -> Result<KitPackage> {
    let create = captured
        .iter()
        .rev()
        .find(|c| c.path == "/sandbox")
        .ok_or_else(|| Error::Compile("sbx did not send a creation request".to_owned()))?;
    let mut template: Value = serde_json::from_slice(&create.body)
        .map_err(|e| Error::Compile(format!("invalid creation request: {e}")))?;
    template["name"] = json!("{{name}}");
    if let Some(object) = template.as_object_mut() {
        // Paths of the compiling machine, meaningless elsewhere: its SSH agent,
        // and its credential bindings file, under the home directory of the
        // user (`CreateRequest::with_bindings_path` sets one back).
        object.remove("ssh_agent_socket_path");
        object.remove("bindings_path");
    }

    let mut images = Vec::new();
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for load in captured.iter().filter(|c| c.path == "/docker/images/load") {
        let (reference, manifest_digest) = oci_reference(&load.body).unwrap_or_default();
        if !seen.insert(manifest_digest.clone()) {
            continue;
        }
        entries.push(json!({ "manifest_digest": manifest_digest, "bytes": load.body.len() }));
        images.push(PackageImage::in_memory(reference, load.body.clone()));
    }

    let metadata = json!({
        "compiled_with": compiled_with,
        "base": job.base,
        "mixins": job.mixins,
        "kit_args": job.args,
        "images": entries,
    });
    Ok(KitPackage::in_memory(template, metadata, images)?)
}

/// `(reference name, manifest digest)` read from the `index.json` of an OCI
/// image layout archive.
fn oci_reference(archive: &[u8]) -> Option<(String, String)> {
    let mut tar = tar::Archive::new(archive);
    let mut entry = tar
        .entries()
        .ok()?
        .filter_map(std::result::Result::ok)
        .find(|e| e.path().is_ok_and(|p| p == Path::new("index.json")))?;
    let mut index = String::new();
    entry.read_to_string(&mut index).ok()?;
    let index: Value = serde_json::from_str(&index).ok()?;
    let manifest = index.get("manifests")?.get(0)?;
    Some((
        manifest
            .pointer("/annotations/org.opencontainers.image.ref.name")?
            .as_str()?
            .to_owned(),
        manifest.get("digest")?.as_str()?.to_owned(),
    ))
}

/// A [`docker_sandbox::Error::Package`] about `path`, as an [`Error::Client`].
fn package_error(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::Client(docker_sandbox::Error::Package {
        path: path.to_owned(),
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tar archive of `(path, content)` entries.
    fn tar_of(files: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, content.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// An OCI image layout whose index names `reference`.
    fn oci_layout(reference: &str, digest: &str) -> Vec<u8> {
        let index = json!({ "manifests": [{ "digest": digest, "annotations": { "org.opencontainers.image.ref.name": reference } }] }).to_string();
        tar_of(&[("oci-layout", "{}"), ("index.json", &index)])
    }

    /// The reference and digest come from the index of the layout.
    #[test]
    fn oci_references_are_read_from_the_index() {
        let archive = oci_layout("sbx-kit-src:spec-abc", "sha256:1");
        assert_eq!(
            oci_reference(&archive),
            Some(("sbx-kit-src:spec-abc".into(), "sha256:1".into()))
        );
        assert_eq!(oci_reference(b"not a tar"), None);
    }

    /// A package written from captured requests opens as a `KitPackage`.
    #[test]
    fn packages_are_written_from_captured_requests() {
        let dir = tempfile::tempdir().unwrap();
        // A name the staging directory of the package once took for itself.
        let job =
            CompileJob::new("shell", dir.path().join("pkg-1.2.staging")).with_mixin("/kits/m");
        let create = json!({ "name": "sbx-kit-compile-1", "agent": "shell", "kits": ["a", "b"], "kit_artifacts": [{}, {}], "ssh_agent_socket_path": "/run/x",
                              "bindings_path": "/home/agent/.config/sbx/credentials.yaml" });
        let archive = oci_layout("sbx-kit-src:spec-abc", "sha256:1");
        let captured = vec![
            CapturedRequest {
                path: "/docker/images/load".into(),
                body: archive.clone().into(),
            },
            CapturedRequest {
                path: "/docker/images/load".into(),
                body: archive.into(),
            },
            CapturedRequest {
                path: "/sandbox".into(),
                body: create.to_string().into(),
            },
        ];

        let built = build_package(&job, &captured, &json!({ "sbx": "v0.54.0" })).unwrap();
        built.save(&job.output).unwrap();

        let package = KitPackage::open(&job.output).unwrap();
        assert_eq!(package.compiled_with(), Some("v0.54.0"));
        assert_eq!(package.images().len(), 1, "duplicate loads are stored once");
        assert_eq!(
            package.images()[0].reference.as_deref(),
            Some("sbx-kit-src:spec-abc")
        );
        let request = package.request("job-1").unwrap();
        assert_eq!(request.name, "job-1");
        assert!(
            request.ssh_agent_socket_path.is_none() && request.bindings_path.is_none(),
            "host-specific fields are dropped"
        );
    }

    /// Captures without a creation request are a compilation error.
    #[test]
    fn a_capture_without_creation_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let job = CompileJob::new("shell", dir.path().join("pkg"));
        let error = build_package(&job, &[], &json!({})).unwrap_err();
        assert!(matches!(error, Error::Compile(_)), "{error}");
    }
}
