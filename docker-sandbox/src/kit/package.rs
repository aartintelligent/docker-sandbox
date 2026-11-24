//! Reading and replaying a compiled kit package.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::KitAgent;
use crate::client::Client;
use crate::error::{Error, Result};
use crate::sandbox::{CreateRequest, Created, Sandbox, SwapOutcome, SwapRequest};

/// An image shipped with a kit package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageImage {
    /// Archive to load with `POST /docker/images/load`.
    pub path: PathBuf,
    /// Reference the archive is loaded under, when recorded in `kit.json`.
    pub reference: Option<String>,
}

/// A compiled kit package.
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
    /// Directory the package was read from.
    dir: PathBuf,
    /// Creation request template, `"name": "{{name}}"`.
    template: Value,
    /// `sbx` version the package was compiled with.
    compiled_with: Option<String>,
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
            path if path.exists() => Some(read_json(&path)?),
            _ => None,
        };
        let compiled_with = metadata
            .as_ref()
            .and_then(|m| m.pointer("/compiled_with/sbx"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let images = match metadata
            .as_ref()
            .and_then(|m| m.get("images"))
            .and_then(Value::as_array)
        {
            Some(entries) => images_from_metadata(&dir, entries)?,
            None => images_from_directory(&dir)?,
        };
        Ok(Self {
            dir,
            template,
            compiled_with,
            images,
        })
    }

    /// Directory the package was read from.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `sbx` version the package was compiled with, if recorded.
    #[must_use]
    pub fn compiled_with(&self) -> Option<&str> {
        self.compiled_with.as_deref()
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
            client.images().load_file(&image.path).await?;
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
    /// # Errors
    ///
    /// See [`KitPackage::load_images`] and
    /// [`Sandbox::swap_container`](crate::sandbox::Sandbox::swap_container).
    pub async fn add_to(&self, sandbox: &Sandbox) -> Result<SwapOutcome> {
        self.load_images(sandbox.client()).await?;
        let request = SwapRequest {
            docker_name_override: format!("{}-swap-{}", sandbox.name(), unique_suffix()),
            kit_artifacts: self.kit_artifacts(),
            kit_refs: self.kit_refs(),
        };
        sandbox.swap_container(&request).await
    }

    /// Logs a warning when the daemon runs another `sbx` version than the one
    /// the package was compiled with.
    fn warn_on_version_mismatch(&self, client: &Client) {
        if let Some(compiled) = self.compiled_with.as_deref() {
            let running = client.health().version.as_str();
            if compiled != running {
                tracing::warn!(package = %self.dir.display(), compiled, running, "kit package compiled for another sbx version");
            }
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
            Ok(PackageImage { path, reference })
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
            path,
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
        assert_eq!(package.dir(), dir.path());
        assert_eq!(package.compiled_with(), None);
        assert!(package.images().is_empty(), "no images directory");
        assert_eq!(package.agent().unwrap().name, "shell");

        write(&dir.path().join("images/01.oci.tar"), "b");
        write(&dir.path().join("images/00.oci.tar"), "a");
        write(&dir.path().join("images/README.md"), "not an image");
        let package = KitPackage::open(dir.path()).unwrap();
        let names: Vec<_> = package
            .images()
            .iter()
            .map(|image| image.path.file_name().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["00.oci.tar", "01.oci.tar"]);
        assert!(
            package
                .images()
                .iter()
                .all(|image| image.reference.is_none())
        );
    }
}
