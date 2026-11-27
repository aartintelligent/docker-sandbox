//! Reading, holding and replaying a kit package.
//!
//! A package lives in a directory ([`KitPackage::open`]) or in memory
//! ([`KitPackage::in_memory`]), as a kit forger writes it: nothing then
//! reaches the disk, and [`KitPackage::save`] writes it when it is to be
//! kept.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde_json::{Map, Value};

use super::KitAgent;
use crate::client::Client;
use crate::error::{Error, Result};
use crate::sandbox::{CreateRequest, Created, Sandbox, SwapOutcome, SwapRequest};

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
    /// returns it as read from there. It is written beside `dir` under
    /// another name, then renamed: a package is never found half written.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Package`] when the package cannot be written, an
    /// archive of its own directory included.
    pub fn save(&self, dir: impl Into<PathBuf>) -> Result<Self> {
        self.save_to(dir.into())
    }

    /// [`KitPackage::save`], compiled once whatever the type of path.
    fn save_to(&self, output: PathBuf) -> Result<Self> {
        let staging = output.with_extension("staging");
        let failed = |error: std::io::Error| package_error(&staging, &error.to_string());
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(staging.join("images")).map_err(failed)?;
        let recorded = self.metadata.get("images").and_then(Value::as_array);
        let mut entries = Vec::new();
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
            entry.insert("file".to_owned(), Value::String(file));
            if let Some(reference) = &image.reference {
                entry.insert("ref".to_owned(), Value::String(reference.clone()));
            }
            entries.push(Value::Object(entry));
        }
        let mut metadata = self.metadata.clone();
        metadata.insert("images".to_owned(), Value::Array(entries));
        // Serializing a `Value` cannot fail: its map keys are strings.
        let pretty = |value: &Value| serde_json::to_vec_pretty(value).unwrap_or_default();
        for (file, value) in [
            ("create-body.json", self.template.clone()),
            ("kit.json", Value::Object(metadata)),
        ] {
            std::fs::write(staging.join(file), pretty(&value)).map_err(failed)?;
        }
        let _ = std::fs::remove_dir_all(&output);
        std::fs::rename(&staging, &output).map_err(|e| package_error(&output, &e.to_string()))?;
        Self::open(output)
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
            let reference = entry
                .get("ref")
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
                .map(str::to_owned);
            Ok(PackageImage {
                archive: ImageArchive::File(path),
                reference,
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
        assert!(!output.with_extension("staging").exists());
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

        // A staging directory left read-only by another user cannot be
        // emptied nor written to; the tests do not run as root.
        let output = dir.path().join("locked");
        let staging = output.with_extension("staging");
        std::fs::create_dir_all(staging.join("images")).unwrap();
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o555)).unwrap();
        let error = refusal(&output);
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            error.starts_with(&format!("invalid kit package at {}:", staging.display())),
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
