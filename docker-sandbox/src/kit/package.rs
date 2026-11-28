//! Reading, holding and replaying a kit package.
//!
//! A package lives in a directory ([`KitPackage::open`]) or in memory
//! ([`KitPackage::in_memory`]), as a kit forger writes it: nothing then
//! reaches the disk, and [`KitPackage::save`] writes it when it is to be
//! kept.

use std::collections::BTreeSet;
use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde_json::{Map, Value};

use super::KitAgent;
use crate::client::Client;
use crate::daemon::Health;
use crate::error::{Error, Result};
use crate::sandbox::{CreateRequest, Created, Sandbox, SwapOutcome, SwapRequest};

/// How many times a save renames its package into place while other
/// writers of the same directory put theirs there, between its removal of
/// the directory and its rename: the last writer wins.
const PLACE_ATTEMPTS: usize = 16;

/// The saves of this process, counted so that each stages its package under
/// a name of its own.
static SAVES: AtomicU64 = AtomicU64::new(0);

/// The name the forge of `docker-sandbox-kit` records itself under in
/// `kit.json` (`forged_by`).
const FORGE: &str = "docker-sandbox-kit";

/// An image shipped with a kit package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageImage {
    /// The archive to load with `POST /docker/images/load`.
    pub archive: ImageArchive,
    /// Reference the archive is loaded under, when recorded in `kit.json`:
    /// an image the daemon already holds under it is not loaded again.
    pub reference: Option<String>,
}

/// Where the archive of a [`PackageImage`] is.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageArchive {
    /// A file of the directory of the package.
    File(PathBuf),
    /// The archive itself, for a package held in memory.
    Memory(Bytes),
}

impl PackageImage {
    /// An image whose archive `archive` is held in memory, loaded under
    /// `reference` when the daemon does not hold it yet.
    pub fn in_memory(reference: impl Into<String>, archive: impl Into<Bytes>) -> Self {
        Self {
            archive: ImageArchive::Memory(archive.into()),
            reference: Some(reference.into()),
        }
    }
}

impl fmt::Debug for ImageArchive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            Self::Memory(bytes) => write!(f, "Memory({} bytes)", bytes.len()),
        }
    }
}

/// A kit package: a creation request template whose kit artifacts are
/// resolved, and the images of its local kits.
///
/// ```no_run
/// use docker_sandbox::Client;
/// use docker_sandbox::kit::KitPackage;
///
/// # async fn run() -> docker_sandbox::Result<()> {
/// let client = Client::connect().await?;
/// let package = KitPackage::open("/var/lib/kits/shell-session")?;
/// let request = package.request("job-42")?.with_cpus(2).with_env("CI", "true");
/// let created = package.create_with(&client, &request).await?;
/// println!("created {} ({})", created.name, created.id);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct KitPackage {
    /// Directory the package was read from, none for a package in memory.
    dir: Option<PathBuf>,
    /// Creation request template, `"name": "{{name}}"`.
    template: Value,
    /// Its metadata, the content of `kit.json`: an object, empty when none.
    metadata: Map<String, Value>,
    /// `sbx` version the package was compiled with.
    compiled_with: Option<String>,
    /// API version of the daemon the package was compiled or forged for.
    compiled_api: Option<String>,
    /// Image archives shipped with the package.
    images: Vec<PackageImage>,
}

impl KitPackage {
    /// Reads a package from `dir`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Package`] if `create-body.json` is missing or invalid,
    /// or if an image archive listed in `kit.json` is missing.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        let template = read_json(&dir.join("create-body.json"))?;
        if !template.is_object() {
            return Err(package_error(&dir, "create-body.json is not a JSON object"));
        }
        let metadata = match dir.join("kit.json") {
            path if path.exists() => match read_json(&path)? {
                Value::Object(metadata) => metadata,
                _ => return Err(package_error(&path, "kit.json is not a JSON object")),
            },
            _ => Map::new(),
        };
        let images = match metadata.get("images").and_then(Value::as_array) {
            Some(entries) => images_from_metadata(&dir, entries)?,
            None => images_from_directory(&dir)?,
        };
        Ok(Self::assembled(Some(dir), template, metadata, images))
    }

    /// A package held in memory, as a kit forger writes it: the creation
    /// request template `template` (`"name": "{{name}}"`), its metadata
    /// `metadata` (what `kit.json` holds, `null` for none) and its images.
    /// Nothing is written: [`KitPackage::save`] writes it to a directory.
    ///
    /// ```
    /// use docker_sandbox::kit::{KitPackage, PackageImage};
    ///
    /// let template = serde_json::json!({ "name": "{{name}}", "agent": "shell", "kits": [], "kit_artifacts": [] });
    /// let image = PackageImage::in_memory("sbx-kit-src:spec-abc", b"an OCI archive".to_vec());
    /// let package = KitPackage::in_memory(template, serde_json::Value::Null, [image])?;
    /// assert_eq!(package.dir(), None);
    /// assert_eq!(package.request("job-42")?.name, "job-42");
    /// # Ok::<(), docker_sandbox::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when `template` is not a JSON object,
    /// or `metadata` neither an object nor `null`.
    pub fn in_memory(
        template: Value,
        metadata: Value,
        images: impl IntoIterator<Item = PackageImage>,
    ) -> Result<Self> {
        Self::checked(template, metadata, images.into_iter().collect())
    }

    /// [`KitPackage::in_memory`], compiled once whatever the type of its
    /// images.
    fn checked(template: Value, metadata: Value, images: Vec<PackageImage>) -> Result<Self> {
        if !template.is_object() {
            return Err(Error::InvalidRequest(
                "the template of a kit package is a JSON object".to_owned(),
            ));
        }
        let metadata = match metadata {
            Value::Object(metadata) => metadata,
            Value::Null => Map::new(),
            _ => {
                return Err(Error::InvalidRequest(
                    "the metadata of a kit package is a JSON object".to_owned(),
                ));
            }
        };
        Ok(Self::assembled(None, template, metadata, images))
    }

    /// The package of `dir`, `template`, `metadata` and `images`, the versions
    /// it records read from its metadata.
    fn assembled(
        dir: Option<PathBuf>,
        template: Value,
        metadata: Map<String, Value>,
        images: Vec<PackageImage>,
    ) -> Self {
        let recorded = |key: &str| {
            metadata
                .get("compiled_with")
                .and_then(|compiled| compiled.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let compiled_with = recorded("sbx");
        let compiled_api = recorded("api_version");
        Self {
            dir,
            template,
            metadata,
            compiled_with,
            compiled_api,
            images,
        }
    }

    /// Directory the package was read from or saved to; none for a package
    /// held in memory.
    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Writes the package to `dir`, replacing a package already there, and
    /// returns it as written there. It is written beside `dir`, under a name
    /// no other save shares (`<name of dir>.staging-<process>-<save>`), then
    /// renamed: a package is never found half written, and saves to
    /// different directories, or to the same one, at once, from one process
    /// or several, never mix their files. When several put a package in the
    /// same directory at once, the last one wins, and a reader may find no
    /// package there while one replaces another. A save that fails leaves
    /// nothing beside `dir`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Package`] when the package cannot be written, an
    /// archive of its own directory included, or `dir` has no name of its
    /// own (`/`, a path ending in `..`).
    pub fn save(&self, dir: impl Into<PathBuf>) -> Result<Self> {
        self.save_to(dir.into())
    }

    /// [`KitPackage::save`], compiled once whatever the type of path.
    fn save_to(&self, output: PathBuf) -> Result<Self> {
        let staging = Staging::beside(&output)?;
        let (metadata, images) = self.write_to(staging.path(), &output)?;
        staging.place(&output)?;
        Ok(Self::assembled(
            Some(output),
            self.template.clone(),
            metadata,
            images,
        ))
    }

    /// Writes the files of the package into `staging`, and returns its
    /// metadata and its images as they are once `staging` is renamed to
    /// `output`.
    fn write_to(
        &self,
        staging: &Path,
        output: &Path,
    ) -> Result<(Map<String, Value>, Vec<PackageImage>)> {
        let failed = |error: std::io::Error| package_error(staging, &error.to_string());
        std::fs::create_dir_all(staging.join("images")).map_err(failed)?;
        let recorded = self.metadata.get("images").and_then(Value::as_array);
        let mut entries = Vec::new();
        let mut images = Vec::new();
        for (index, image) in self.images.iter().enumerate() {
            let file = format!("images/{index:02}.oci.tar");
            let target = staging.join(&file);
            match &image.archive {
                ImageArchive::File(path) => std::fs::copy(path, &target).map(drop),
                ImageArchive::Memory(bytes) => std::fs::write(&target, bytes),
            }
            .map_err(failed)?;
            // The entry recorded for the image, its other fields kept.
            let mut entry = recorded
                .and_then(|entries| entries.get(index))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if let Some(reference) = &image.reference {
                entry.insert("ref".to_owned(), Value::String(reference.clone()));
            }
            // The image as `KitPackage::open` reads it from the entry.
            images.push(PackageImage {
                archive: ImageArchive::File(output.join(&file)),
                reference: recorded_reference(entry.get("ref")),
            });
            entry.insert("file".to_owned(), Value::String(file));
            entries.push(Value::Object(entry));
        }
        let mut metadata = self.metadata.clone();
        metadata.insert("images".to_owned(), Value::Array(entries));
        // Serializing JSON values cannot fail: their map keys are strings.
        for (file, json) in [
            (
                "create-body.json",
                serde_json::to_vec_pretty(&self.template),
            ),
            ("kit.json", serde_json::to_vec_pretty(&metadata)),
        ] {
            std::fs::write(staging.join(file), json.unwrap_or_default()).map_err(failed)?;
        }
        Ok((metadata, images))
    }

    /// `sbx` version the package was compiled with, if recorded.
    #[must_use]
    pub fn compiled_with(&self) -> Option<&str> {
        self.compiled_with.as_deref()
    }

    /// API version of the daemon the package was compiled or forged for, if
    /// recorded. A forged package records no `sbx` version, only this.
    #[must_use]
    pub fn compiled_api(&self) -> Option<&str> {
        self.compiled_api.as_deref()
    }

    /// Whether the package can still create sandboxes on the daemon of
    /// `health`, as recorded when it was written: one the `sbx` CLI compiled
    /// suits the daemon of the same `sbx` version; one `docker-sandbox-kit`
    /// forged suits a daemon of the same API, forged by the version of
    /// `docker-sandbox-kit` that goes with this crate (the two crates share
    /// one version), since another may write its kits otherwise. A package
    /// that records neither suits no daemon: a cache makes it again.
    ///
    /// ```no_run
    /// # async fn run(client: docker_sandbox::Client) -> docker_sandbox::Result<()> {
    /// use docker_sandbox::kit::KitPackage;
    ///
    /// let package = KitPackage::open("/var/lib/kits/shell-session")?;
    /// if !package.suits(client.health()) {
    ///     eprintln!("the package is to be made again for this daemon");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn suits(&self, health: &Health) -> bool {
        if let Some(compiled) = self.compiled_with.as_deref() {
            return compiled == health.version;
        }
        let forger = self.metadata.get("forged_by");
        let field = |key: &str| forger.and_then(|f| f.get(key)).and_then(Value::as_str);
        field("name") == Some(FORGE)
            && field("version") == Some(env!("CARGO_PKG_VERSION"))
            && self.compiled_api.as_deref() == Some(health.api_version.as_str())
    }

    /// Kit references of the package, base kit first.
    #[must_use]
    pub fn kit_refs(&self) -> Vec<String> {
        self.template
            .get("kits")
            .and_then(Value::as_array)
            .map(|kits| {
                kits.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The agent of the package, as its base kit declares it: program, run
    /// options and network destinations.
    #[must_use]
    pub fn agent(&self) -> Option<KitAgent> {
        self.kit_artifacts()
            .first()
            .and_then(KitAgent::from_artifact)
    }

    /// Resolved kit artifacts of the package, one per kit reference, base kit
    /// first. Opaque.
    #[must_use]
    pub fn kit_artifacts(&self) -> Vec<Value> {
        self.template
            .get("kit_artifacts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    /// Image archives shipped with the package.
    #[must_use]
    pub fn images(&self) -> &[PackageImage] {
        &self.images
    }

    /// A creation request for a sandbox named `name`, to customise before
    /// passing it to [`KitPackage::create_with`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the template is not a valid request.
    pub fn request(&self, name: impl Into<String>) -> Result<CreateRequest> {
        CreateRequest::from_template(self.template.clone(), name)
    }

    /// Loads the package images that the daemon does not hold yet. Returns the
    /// number of archives loaded.
    ///
    /// # Errors
    ///
    /// Fails if an archive cannot be read or is rejected by the daemon.
    pub async fn load_images(&self, client: &Client) -> Result<usize> {
        self.warn_on_version_mismatch(client);
        let present: BTreeSet<String> = client
            .images()
            .list()
            .await?
            .iter()
            .map(|i| normalize_reference(&i.reference()))
            .collect();

        let mut loaded = 0;
        for image in &self.images {
            if image
                .reference
                .as_deref()
                .is_some_and(|r| present.contains(&normalize_reference(r)))
            {
                continue;
            }
            match &image.archive {
                ImageArchive::File(path) => client.images().load_file(path).await?,
                ImageArchive::Memory(bytes) => client.images().load(bytes.clone()).await?,
            }
            loaded += 1;
        }
        Ok(loaded)
    }

    /// Loads the package images and creates a sandbox named `name`.
    ///
    /// # Errors
    ///
    /// See [`KitPackage::load_images`] and
    /// [`Sandboxes::create`](crate::sandbox::Sandboxes::create).
    pub async fn create(&self, client: &Client, name: impl Into<String>) -> Result<Created> {
        self.create_with(client, &self.request(name)?).await
    }

    /// Loads the package images and creates a sandbox from a customised request
    /// obtained with [`KitPackage::request`].
    ///
    /// # Errors
    ///
    /// See [`KitPackage::load_images`] and
    /// [`Sandboxes::create`](crate::sandbox::Sandboxes::create).
    pub async fn create_with(&self, client: &Client, request: &CreateRequest) -> Result<Created> {
        self.load_images(client).await?;
        client.sandboxes().create(request).await
    }

    /// Installs the mixins of this package into the running `sandbox`, in
    /// place: their install commands run in the current container, which is
    /// not recreated, and the kit list of the sandbox is not updated. Returns
    /// the number of mixins installed.
    ///
    /// Faster than [`KitPackage::add_to`]; prefer `add_to` when the kits must
    /// survive a recreation of the container or show in the sandbox
    /// description. The base kit of the package (its first kit) is skipped:
    /// the sandbox already has one.
    ///
    /// # Errors
    ///
    /// See [`KitPackage::load_images`] and
    /// [`Sandbox::add_kit_artifact`](crate::sandbox::Sandbox::add_kit_artifact).
    pub async fn install_into(&self, sandbox: &Sandbox) -> Result<usize> {
        self.load_images(sandbox.client()).await?;
        let mixins = self.kit_artifacts().into_iter().skip(1);
        let mut installed = 0;
        for artifact in mixins {
            sandbox.add_kit_artifact(&artifact).await?;
            installed += 1;
        }
        Ok(installed)
    }

    /// Recreates the existing `sandbox` with the kit list of this package,
    /// keeping the changes made inside it. Use it to add mixins to a sandbox
    /// whose base kit matches the package's.
    ///
    /// Nothing asks for an approval: the network rules of the added mixins
    /// apply at once, where `sbx kit add` asks the user first.
    ///
    /// # Errors
    ///
    /// See [`KitPackage::load_images`] and
    /// [`Sandbox::swap_container`](crate::sandbox::Sandbox::swap_container),
    /// which is refused when the mixins would change the credentials of the
    /// sandbox.
    pub async fn add_to(&self, sandbox: &Sandbox) -> Result<SwapOutcome> {
        self.load_images(sandbox.client()).await?;
        let request = SwapRequest {
            docker_name_override: format!("{}-swap-{}", sandbox.name(), unique_suffix()),
            kit_artifacts: self.kit_artifacts(),
            kit_refs: self.kit_refs(),
        };
        sandbox.swap_container(&request).await
    }

    /// Logs a warning when the daemon runs another `sbx` version, or speaks
    /// another API, than the package was compiled or forged for.
    fn warn_on_version_mismatch(&self, client: &Client) {
        let health = client.health();
        if let Some(compiled) = self.compiled_with.as_deref()
            && compiled != health.version
        {
            tracing::warn!(package = ?self.dir, compiled, running = %health.version, "kit package compiled for another sbx version");
        }
        if let Some(compiled) = self.compiled_api.as_deref()
            && compiled != health.api_version
        {
            tracing::warn!(package = ?self.dir, compiled, running = %health.api_version, "kit package written for another daemon API");
        }
    }
}

/// The directory a package is written to before it is renamed into place:
/// beside the directory it is saved to, under a name no other save shares,
/// and removed with what it holds unless it was put in place.
struct Staging {
    /// Its path.
    path: PathBuf,
    /// Whether it was renamed into place.
    placed: bool,
}

impl Staging {
    /// The staging directory of a save to `output`: the name of `output`
    /// followed by `.staging-<process>-<save>`, in the same directory, so
    /// that the rename stays on one file system.
    fn beside(output: &Path) -> Result<Self> {
        let Some(name) = output.file_name() else {
            return Err(package_error(
                output,
                "a kit package is saved to a directory of a name of its own",
            ));
        };
        let mut staged = name.to_os_string();
        staged.push(format!(
            ".staging-{}-{}",
            std::process::id(),
            SAVES.fetch_add(1, Ordering::Relaxed)
        ));
        let path = output.with_file_name(staged);
        // Only a process that ended under the same identifier can have left
        // a directory of that name.
        let _ = std::fs::remove_dir_all(&path);
        Ok(Self {
            path,
            placed: false,
        })
    }

    /// Its path.
    fn path(&self) -> &Path {
        &self.path
    }

    /// Renames the staging directory to `output`, replacing what is there.
    /// A writer that puts its package in `output` between the removal and
    /// the rename makes the rename fail: the package is removed again, so
    /// that the last writer wins.
    fn place(mut self, output: &Path) -> Result<()> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            let _ = std::fs::remove_dir_all(output);
            match std::fs::rename(&self.path, output) {
                Ok(()) => {
                    self.placed = true;
                    return Ok(());
                }
                Err(error)
                    if attempts < PLACE_ATTEMPTS
                        && matches!(
                            error.kind(),
                            ErrorKind::DirectoryNotEmpty | ErrorKind::AlreadyExists
                        ) => {}
                Err(error) => return Err(package_error(output, &error.to_string())),
            }
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.placed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// `docker.io/library/name:tag` and `name:tag` designate the same image.
fn normalize_reference(reference: &str) -> String {
    reference
        .strip_prefix("docker.io/library/")
        .unwrap_or(reference)
        .to_owned()
}

/// Eight hexadecimal characters, unique enough for a temporary container name.
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("{:08x}", nanos ^ std::process::id().rotate_left(16))
}

/// Reads a JSON file of the package.
fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).map_err(|e| package_error(path, &e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| package_error(path, &format!("invalid JSON: {e}")))
}

/// The reference an image entry of `kit.json` records, from its `ref`: none
/// when it is missing, empty or not a string.
fn recorded_reference(reference: Option<&Value>) -> Option<String> {
    reference
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
        .map(str::to_owned)
}

/// Image archives listed in `kit.json`.
fn images_from_metadata(dir: &Path, entries: &[Value]) -> Result<Vec<PackageImage>> {
    entries
        .iter()
        .map(|entry| {
            let file = entry
                .get("file")
                .and_then(Value::as_str)
                .ok_or_else(|| package_error(dir, "image entry without a file"))?;
            let path = dir.join(file);
            if !path.is_file() {
                return Err(package_error(
                    &path,
                    "image archive listed in kit.json is missing",
                ));
            }
            Ok(PackageImage {
                archive: ImageArchive::File(path),
                reference: recorded_reference(entry.get("ref")),
            })
        })
        .collect()
}

/// Image archives found in `images/`, for packages without metadata.
fn images_from_directory(dir: &Path) -> Result<Vec<PackageImage>> {
    let images_dir = dir.join("images");
    if !images_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&images_dir)
        .map_err(|e| package_error(&images_dir, &e.to_string()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "tar"))
        .collect();
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|path| PackageImage {
            archive: ImageArchive::File(path),
            reference: None,
        })
        .collect())
}

/// An [`Error::Package`] about `path`.
fn package_error(path: &Path, message: &str) -> Error {
    Error::Package {
        path: path.to_owned(),
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// Writes `content` to `path`, creating its directory.
    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn package_layout_is_read() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("create-body.json"),
            r#"{"name":"{{name}}","agent":"shell","kits":["docker.io/sbx/shell@sha256:f3","/kits/mixin"],"kit_artifacts":[{},{}]}"#,
        );
        write(
            &dir.path().join("kit.json"),
            r#"{"compiled_with":{"sbx":"v0.54.0"},"images":[{"file":"images/00.oci.tar","ref":"sbx-kit-src:spec-abc"}]}"#,
        );
        write(&dir.path().join("images/00.oci.tar"), "archive");

        let package = KitPackage::open(dir.path()).unwrap();
        assert_eq!(package.compiled_with(), Some("v0.54.0"));
        assert_eq!(
            package.kit_refs(),
            ["docker.io/sbx/shell@sha256:f3", "/kits/mixin"]
        );
        assert_eq!(
            package.images()[0].reference.as_deref(),
            Some("sbx-kit-src:spec-abc")
        );
        assert_eq!(package.request("job-42").unwrap().name, "job-42");
    }

    #[test]
    fn missing_pieces_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            KitPackage::open(dir.path()),
            Err(Error::Package { .. })
        ));

        write(
            &dir.path().join("create-body.json"),
            r#"{"name":"{{name}}","agent":"shell"}"#,
        );
        write(
            &dir.path().join("kit.json"),
            r#"{"images":[{"file":"images/missing.oci.tar"}]}"#,
        );
        assert!(matches!(
            KitPackage::open(dir.path()),
            Err(Error::Package { .. })
        ));
    }

    #[test]
    fn references_are_normalized() {
        assert_eq!(
            normalize_reference("docker.io/library/sbx-kit-src:spec-1"),
            "sbx-kit-src:spec-1"
        );
        assert_eq!(
            normalize_reference("docker.io/sbx/shell@sha256:f3"),
            "docker.io/sbx/shell@sha256:f3"
        );
        assert_eq!(unique_suffix().len(), 8);
    }

    #[test]
    fn templates_must_be_json_objects() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("create-body.json"), "[]");
        assert!(matches!(
            KitPackage::open(dir.path()),
            Err(Error::Package { ref message, .. }) if message.contains("not a JSON object")
        ));
    }

    #[test]
    fn packages_without_metadata_ship_the_archives_of_their_images_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("create-body.json"),
            r#"{"name":"{{name}}","agent":"shell","kit_artifacts":[{"manifest":{"name":"shell"}}]}"#,
        );
        let package = KitPackage::open(dir.path()).unwrap();
        assert_eq!(package.dir(), Some(dir.path()));
        assert_eq!(package.compiled_with(), None);
        assert!(package.images().is_empty(), "no images directory");
        assert_eq!(package.agent().unwrap().name, "shell");

        write(&dir.path().join("images/01.oci.tar"), "b");
        write(&dir.path().join("images/00.oci.tar"), "a");
        write(&dir.path().join("images/README.md"), "not an image");
        let package = KitPackage::open(dir.path()).unwrap();
        let archives: Vec<_> = package
            .images()
            .iter()
            .map(|image| image.archive.clone())
            .collect();
        assert_eq!(
            archives,
            ["00.oci.tar", "01.oci.tar"]
                .map(|file| ImageArchive::File(dir.path().join("images").join(file)))
        );
        assert!(
            package
                .images()
                .iter()
                .all(|image| image.reference.is_none())
        );
    }

    /// A package in memory with one image, `a`, under `ref-a`.
    fn in_memory() -> KitPackage {
        let template = serde_json::json!({ "name": "{{name}}", "agent": "shell" });
        let metadata = serde_json::json!({ "forged_by": { "name": "test" }, "compiled_with": { "api_version": "0.52.0" },
                                           "images": [{ "file": "images/00.oci.tar", "ref": "ref-a", "bytes": 1 }] });
        KitPackage::in_memory(
            template,
            metadata,
            [PackageImage::in_memory("ref-a", b"a".to_vec())],
        )
        .unwrap()
    }

    #[test]
    fn packages_in_memory_are_checked_and_have_no_directory() {
        let package = in_memory();
        assert_eq!(package.dir(), None);
        assert_eq!(package.compiled_api(), Some("0.52.0"));
        assert_eq!(
            format!("{:?}", package.images()[0].archive),
            "Memory(1 bytes)"
        );
        assert_eq!(
            format!("{:?}", ImageArchive::File(PathBuf::from("x.tar"))),
            "File(\"x.tar\")"
        );
        let empty = KitPackage::in_memory(serde_json::json!({}), Value::Null, []).unwrap();
        assert!(empty.images().is_empty() && empty.compiled_with().is_none());

        let refused = KitPackage::in_memory(serde_json::json!([]), Value::Null, []).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("the template of a kit package is a JSON object")
        );
        let refused =
            KitPackage::in_memory(serde_json::json!({}), serde_json::json!([]), []).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("the metadata of a kit package is a JSON object")
        );
    }

    #[test]
    fn packages_suit_the_daemon_they_were_written_for() {
        let health = |version: &str, api: &str| -> Health {
            serde_json::from_value(
                serde_json::json!({ "status": "healthy", "version": version, "api_version": api }),
            )
            .unwrap()
        };
        let package = |metadata: Value| {
            KitPackage::in_memory(serde_json::json!({ "name": "{{name}}" }), metadata, []).unwrap()
        };
        let compiled = package(
            serde_json::json!({ "compiled_with": { "sbx": "v0.54.1", "api_version": "0.52.0" } }),
        );
        assert!(compiled.suits(&health("v0.54.1", "0.52.0")));
        assert!(!compiled.suits(&health("v0.55.0", "0.52.0")), "another sbx");

        let forged = |name: &str, version: &str| {
            package(
                serde_json::json!({ "forged_by": { "name": name, "version": version },
                                        "compiled_with": { "api_version": "0.52.0" } }),
            )
        };
        let this = env!("CARGO_PKG_VERSION");
        let current = forged(FORGE, this);
        assert!(
            current.suits(&health("v0.55.0", "0.52.0")),
            "another sbx, the same API"
        );
        assert!(!current.suits(&health("v0.55.0", "0.53.0")), "another API");
        assert!(
            !forged(FORGE, "0.0.1").suits(&health("v0.54.1", "0.52.0")),
            "another forge"
        );
        assert!(
            !forged("audit", this).suits(&health("v0.54.1", "0.52.0")),
            "another forger"
        );
        assert!(
            !package(Value::Null).suits(&health("v0.54.1", "0.52.0")),
            "nothing recorded"
        );
    }

    #[test]
    fn packages_are_saved_through_a_staging_directory() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("pkg");
        std::fs::create_dir_all(output.join("old")).unwrap();
        let saved = in_memory().save(&output).unwrap();
        assert_eq!(saved.dir(), Some(output.as_path()));
        assert_eq!(
            std::fs::read(output.join("images/00.oci.tar")).unwrap(),
            b"a"
        );
        assert!(
            !output.join("old").exists(),
            "the previous package is replaced"
        );
        assert_no_staging(dir.path());
        let metadata = read_json(&output.join("kit.json")).unwrap();
        assert_eq!(metadata["forged_by"]["name"], "test");
        assert_eq!(
            metadata["images"],
            serde_json::json!([{ "file": "images/00.oci.tar", "ref": "ref-a", "bytes": 1 }])
        );
        assert_eq!(saved.images()[0].reference.as_deref(), Some("ref-a"));

        // A package of a directory saved to another: its archives are copied.
        let copy = saved.save(dir.path().join("copy")).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("copy/images/00.oci.tar")).unwrap(),
            b"a"
        );
        assert_eq!(copy.compiled_api(), Some("0.52.0"));

        // An image without a reference or an entry gets a file of its own.
        let template = serde_json::json!({ "name": "{{name}}" });
        let bare = PackageImage {
            archive: ImageArchive::Memory(Bytes::from_static(b"b")),
            reference: None,
        };
        let saved = KitPackage::in_memory(template, Value::Null, [bare])
            .unwrap()
            .save(&output)
            .unwrap();
        assert_eq!(
            read_json(&output.join("kit.json")).unwrap()["images"],
            serde_json::json!([{ "file": "images/00.oci.tar" }])
        );
        assert!(saved.images()[0].reference.is_none());
    }

    #[test]
    fn packages_that_cannot_be_saved_are_package_errors() {
        let dir = tempfile::tempdir().unwrap();
        let refusal = |output: &Path| in_memory().save(output).unwrap_err().to_string();

        // The staging directory cannot be made under a file.
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(refusal(&file.join("pkg")).starts_with("invalid kit package at "));

        // The package cannot replace a file.
        let error = refusal(&file);
        assert!(
            error.starts_with(&format!("invalid kit package at {}:", file.display())),
            "{error}"
        );

        // A directory the tests cannot write to holds no staging directory;
        // the tests do not run as root.
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let output = locked.join("pkg");
        let error = refusal(&output);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            error.starts_with(&format!(
                "invalid kit package at {}.staging-",
                output.display()
            )),
            "{error}"
        );

        // A package that cannot be removed is not replaced, and the staging
        // directory goes.
        let stuck = dir.path().join("stuck");
        std::fs::create_dir_all(stuck.join("kept")).unwrap();
        std::fs::write(stuck.join("kept/file"), "x").unwrap();
        std::fs::set_permissions(stuck.join("kept"), std::fs::Permissions::from_mode(0o555))
            .unwrap();
        let error = refusal(&stuck);
        std::fs::set_permissions(stuck.join("kept"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(
            error.starts_with(&format!("invalid kit package at {}:", stuck.display())),
            "{error}"
        );

        // A directory without a name of its own is refused before anything.
        let error = refusal(&dir.path().join("sub/.."));
        assert!(
            error.ends_with("a directory of a name of its own"),
            "{error}"
        );

        // An archive of the directory of a package that went away.
        let gone = dir.path().join("gone");
        let package = in_memory().save(&gone).unwrap();
        std::fs::remove_file(gone.join("images/00.oci.tar")).unwrap();
        let error = package
            .save(dir.path().join("elsewhere"))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("invalid kit package at "), "{error}");
        assert_no_staging(dir.path());
    }

    /// Checks that no staging directory is left in `dir`.
    fn assert_no_staging(dir: &Path) {
        let left: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".staging-"))
            .collect();
        assert!(left.is_empty(), "staging directories left: {left:?}");
    }

    /// A package of one image holding `content`.
    fn holding(content: u8) -> KitPackage {
        let template = serde_json::json!({ "name": "{{name}}" });
        KitPackage::in_memory(
            template,
            Value::Null,
            [PackageImage::in_memory("ref", vec![content])],
        )
        .unwrap()
    }

    #[test]
    fn packages_are_saved_to_directories_whatever_their_name() {
        let dir = tempfile::tempdir().unwrap();
        for (content, name) in (*b"abcd").into_iter().zip([
            "pkg-1.2",
            "pkg-1.3",
            "pkg.staging",
            "shell-1.2-86d50519f269",
        ]) {
            let output = dir.path().join(name);
            holding(content).save(&output).unwrap();
            assert_eq!(
                std::fs::read(output.join("images/00.oci.tar")).unwrap(),
                [content]
            );
        }
        assert_no_staging(dir.path());

        // Two saves of a directory, and saves of directories that differ
        // after their last dot, stage under names of their own.
        let output = dir.path().join("a-1.2");
        let first = Staging::beside(&output).unwrap();
        let second = Staging::beside(&output).unwrap();
        let other = Staging::beside(&dir.path().join("a-1.3")).unwrap();
        let names = [first.path(), second.path(), other.path()];
        assert!(names[0] != names[1] && names[0] != names[2] && names[1] != names[2]);
        assert!(
            names[0]
                .to_string_lossy()
                .starts_with(&format!("{}.staging-", output.display()))
        );
    }

    #[test]
    fn saves_at_once_keep_their_own_packages() {
        let dir = tempfile::tempdir().unwrap();
        // Directories that differ after their last dot, saved at once.
        std::thread::scope(|scope| {
            for content in 0..6 {
                let output = dir.path().join(format!("pkg-1.{content}"));
                scope.spawn(move || holding(content).save(output).unwrap());
            }
        });
        for content in 0..6 {
            let image = dir
                .path()
                .join(format!("pkg-1.{content}/images/00.oci.tar"));
            assert_eq!(std::fs::read(image).unwrap(), [content]);
        }

        // One directory saved at once by six writers: every save succeeds,
        // and the package of one of them is there.
        let shared = dir.path().join("shared");
        let saved: Vec<KitPackage> = std::thread::scope(|scope| {
            let writers: Vec<_> = (0..6)
                .map(|content| {
                    let output = shared.clone();
                    scope.spawn(move || holding(content).save(output).unwrap())
                })
                .collect();
            writers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        assert!(saved.iter().all(|p| p.dir() == Some(shared.as_path())));
        let image = std::fs::read(shared.join("images/00.oci.tar")).unwrap();
        assert!(image.len() == 1 && image[0] < 6, "{image:?}");
        assert_no_staging(dir.path());
    }

    #[test]
    fn metadata_that_is_not_an_object_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("create-body.json"),
            r#"{"name":"{{name}}"}"#,
        );
        write(&dir.path().join("kit.json"), "[]");
        let error = KitPackage::open(dir.path()).unwrap_err().to_string();
        assert!(error.ends_with("kit.json is not a JSON object"), "{error}");
    }
}
