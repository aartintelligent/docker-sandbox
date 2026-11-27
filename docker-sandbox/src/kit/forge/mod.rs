//! Forging kit packages without the `sbx` CLI (feature `kit-forge`).
//!
//! A [`KitPackage`] holds what the daemon needs to create a sandbox from a
//! kit composition: a creation template whose `kit_artifacts` describe each
//! kit, and the source images of the local mixins. `KitCompiler` (feature
//! `kit-compiler`) captures them from the CLI; [`KitForge`] writes them
//! itself, for mixins described by
//! a [`KitDescriptor`], on the base kit of a package compiled before:
//!
//! ```text
//! <output>/kit.json            metadata: forged by this crate, API of the daemon, kits, images
//! <output>/create-body.json    creation request template, "name": "{{name}}"
//! <output>/images/NN.oci.tar   source images of the mixins
//! ```
//!
//! The format of the artifacts and images is the private contract between the
//! CLI and the daemon, measured by the audit of this repository for the `sbx`
//! versions this crate supports ([`SUPPORTED_API`](crate::SUPPORTED_API)).
//! A forged package records the API it was forged for.
//!
//! The CLI checks the kit policy of the daemon before it loads a local kit;
//! forging does too: it refuses when `kit.allowLocalKits` is off, or when
//! `kit.requireSignature` is on, since a forged kit carries no signature.

mod artifact;
mod descriptor;
mod oci;

pub use self::descriptor::{KitDescriptor, LifecycleStep};

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use self::artifact::MixinParts;
use super::KitPackage;
use crate::client::Client;
use crate::error::{Error, Result};

/// Setting of the daemon that allows kits from local directories.
const ALLOW_LOCAL_KITS: &str = "kit.allowLocalKits";

/// Setting of the daemon that requires signed kits.
const REQUIRE_SIGNATURE: &str = "kit.requireSignature";

/// A mixin to forge: its descriptor, and the reference the kit list of the
/// sandbox shows it under.
///
/// ```
/// use docker_sandbox::kit::{KitDescriptor, LifecycleStep, Mixin};
///
/// let descriptor = KitDescriptor::mixin("Acme tools")
///     .with_install(LifecycleStep::new("pip install acme-cli").as_user("agent"));
/// let mixin = Mixin::new("/kits/acme-tools", descriptor)?;
/// assert_eq!(mixin.name(), "acme-tools");
/// # Ok::<(), docker_sandbox::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Mixin {
    /// Reference of the mixin, a path, as the CLI lists a local kit.
    reference: String,
    /// Name of the mixin: the last part of its reference.
    name: String,
    /// The validated descriptor.
    descriptor: KitDescriptor,
    /// Text of its `kit.yaml`: the file read, or the descriptor written out.
    spec: String,
}

impl Mixin {
    /// The mixin of the directory `dir`, described by its `spec.yaml` written
    /// in JSON, and listed under the canonical path of the directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::File`] when the directory or its `spec.yaml` cannot be
    /// read, and [`Error::InvalidKit`] when the descriptor cannot be forged.
    pub fn read(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let file_error = |path: &Path| {
            let path = path.to_owned();
            move |source| Error::File { path, source }
        };
        let canonical = dir.canonicalize().map_err(file_error(dir))?;
        let spec_path = canonical.join("spec.yaml");
        let spec = std::fs::read_to_string(&spec_path).map_err(file_error(&spec_path))?;
        let descriptor = KitDescriptor::parse(&spec).map_err(|error| renamed(error, dir))?;
        let mut mixin = Self::new(canonical.display().to_string(), descriptor)?;
        mixin.spec = spec;
        Ok(mixin)
    }

    /// The mixin `descriptor`, listed under `reference`: a path, as the CLI
    /// lists a local kit, whose last part names the mixin. The daemon finds
    /// the mixin by its source image, so the path need not exist.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidKit`] when the reference names nothing or the
    /// descriptor cannot be forged.
    pub fn new(reference: impl Into<String>, descriptor: KitDescriptor) -> Result<Self> {
        let reference = reference.into();
        let name = reference
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        if name.is_empty() {
            return Err(Error::InvalidKit {
                kit: reference,
                message: "the reference of a mixin is a path that names it".to_owned(),
            });
        }
        descriptor
            .validate()
            .map_err(|error| renamed(error, Path::new(&name)))?;
        Ok(Self {
            spec: descriptor.to_spec(),
            reference,
            name,
            descriptor,
        })
    }

    /// Name of the mixin, the last part of its reference.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Reference the kit list of the sandbox shows the mixin under.
    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// The descriptor of the mixin.
    #[must_use]
    pub fn descriptor(&self) -> &KitDescriptor {
        &self.descriptor
    }
}

/// Writes kit packages without the `sbx` CLI (feature `kit-forge`): mixins
/// forged from their descriptor, on the base kit of a compiled package.
///
/// ```no_run
/// use docker_sandbox::Client;
/// use docker_sandbox::kit::{KitDescriptor, KitForge, KitPackage, LifecycleStep, Mixin};
///
/// # async fn run() -> docker_sandbox::Result<()> {
/// let client = Client::connect().await?;
/// let tools = KitDescriptor::mixin("Acme tools")
///     .with_network_allow(["api.acme.example:443"])
///     .with_install(LifecycleStep::new("pip install acme-cli").as_user("agent"));
/// let package = KitForge::new(client.clone())
///     .with_base_package(KitPackage::open("/var/lib/kits/shell")?)
///     .with_mixin(Mixin::new("/kits/acme-tools", tools)?)
///     .forge("/var/lib/kits/shell+acme-tools")
///     .await?;
/// package.create(&client, "job-42").await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct KitForge {
    /// Client of the daemon the package is forged for.
    client: Client,
    /// Package whose base kit (its first kit) the mixins are composed with.
    base: Option<KitPackage>,
    /// Mixins, in composition order.
    mixins: Vec<Mixin>,
}

impl KitForge {
    /// Forges packages for the daemon of `client`.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            base: None,
            mixins: Vec::new(),
        }
    }

    /// Takes the base kit, the first kit, of `package`: compiled once with
    /// `KitCompiler` (feature `kit-compiler`) or `audit kit-compile`. Its
    /// other kits and their images are left out.
    #[must_use]
    pub fn with_base_package(mut self, package: KitPackage) -> Self {
        self.base = Some(package);
        self
    }

    /// Appends a mixin, composed after the base kit and the mixins before it.
    #[must_use]
    pub fn with_mixin(mut self, mixin: Mixin) -> Self {
        self.mixins.push(mixin);
        self
    }

    /// Writes the package into `output`, replaced when it exists, and opens
    /// it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Forge`] without a base kit or a mixin, when two kits
    /// share a name, or when the kit policy of the daemon refuses local or
    /// unsigned kits; the errors of the settings requests; [`Error::Archive`]
    /// and [`Error::Package`] when the package cannot be written.
    pub async fn forge(&self, output: impl Into<PathBuf>) -> Result<KitPackage> {
        let output = output.into();
        let base = self.base.as_ref().ok_or_else(|| {
            Error::Forge("no base kit: give a compiled package with `with_base_package`".to_owned())
        })?;
        if self.mixins.is_empty() {
            return Err(Error::Forge("no mixin to forge".to_owned()));
        }
        self.check_policy().await?;

        let base_artifact = base.kit_artifacts().into_iter().next().unwrap_or_default();
        let base_name = base_artifact
            .pointer("/manifest/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut names = BTreeSet::from([base_name.to_owned()]);
        for mixin in &self.mixins {
            if !names.insert(mixin.name.clone()) {
                return Err(Error::Forge(format!(
                    "two kits are named {}: each kit of a composition needs its own name",
                    mixin.name
                )));
            }
        }

        let mut kits = vec![base.kit_refs().into_iter().next().unwrap_or_default()];
        let mut artifacts = vec![base_artifact.clone()];
        let mut images = Vec::new();
        for mixin in &self.mixins {
            let image = oci::source_image(&mixin.spec, mixin.descriptor.as_json())?;
            artifacts.push(artifact::mixin(&MixinParts {
                name: &mixin.name,
                reference: &mixin.reference,
                descriptor: mixin.descriptor.as_json(),
                image: &image.reference,
                manifest_digest: &image.manifest_digest,
            }));
            kits.push(mixin.reference.clone());
            images.push(image);
        }

        let mut template = serde_json::to_value(base.request("{{name}}")?)
            .map_err(|e| package_error(base.dir(), e))?;
        template["kits"] = json!(kits);
        template["kit_artifacts"] = Value::Array(artifacts);
        let metadata = json!({
            "forged_by": { "name": "docker-sandbox", "version": env!("CARGO_PKG_VERSION") },
            "compiled_with": { "api_version": self.client.health().api_version },
            "base": kits.first(),
            "mixins": kits.get(1..),
            "images": images.iter().enumerate().map(|(index, image)| json!({
                "file": image_file(index),
                "ref": image.reference,
                "manifest_digest": image.manifest_digest,
                "bytes": image.archive.len(),
            })).collect::<Vec<_>>(),
        });
        let archives: Vec<&[u8]> = images.iter().map(|i| i.archive.as_slice()).collect();
        write_package(&output, &template, &metadata, &archives)?;
        KitPackage::open(&output)
    }

    /// Refuses to forge when the kit policy of the daemon forbids local kits
    /// or requires signed ones. A setting the daemon does not know keeps its
    /// default, which allows forging.
    async fn check_policy(&self) -> Result<()> {
        if self.setting(ALLOW_LOCAL_KITS).await? == Some(Value::Bool(false)) {
            return Err(Error::Forge(format!(
                "the daemon forbids local kits ({ALLOW_LOCAL_KITS} is off)"
            )));
        }
        if self.setting(REQUIRE_SIGNATURE).await? == Some(Value::Bool(true)) {
            return Err(Error::Forge(format!(
                "the daemon requires signed kits ({REQUIRE_SIGNATURE} is on), and a forged kit \
                 carries no signature"
            )));
        }
        Ok(())
    }

    /// The value of the setting `key`, `None` when the daemon does not know
    /// it.
    async fn setting(&self, key: &str) -> Result<Option<Value>> {
        match self.client.daemon().setting(key).await {
            Ok(setting) => Ok(Some(setting.value)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// Path of the `index`-th image archive inside a package.
fn image_file(index: usize) -> String {
    format!("images/{index:02}.oci.tar")
}

/// Writes a package into `output` through a staging directory renamed into
/// place at the end, as the compiler does.
fn write_package(
    output: &Path,
    template: &Value,
    metadata: &Value,
    archives: &[&[u8]],
) -> Result<()> {
    let staging = output.with_extension("staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(staging.join("images")).map_err(|e| package_error(&staging, e))?;
    for (index, archive) in archives.iter().enumerate() {
        std::fs::write(staging.join(image_file(index)), archive)
            .map_err(|e| package_error(&staging, e))?;
    }
    for (file, value) in [("create-body.json", template), ("kit.json", metadata)] {
        let bytes = serde_json::to_vec_pretty(value).map_err(|e| package_error(&staging, e))?;
        std::fs::write(staging.join(file), bytes).map_err(|e| package_error(&staging, e))?;
    }
    let _ = std::fs::remove_dir_all(output);
    std::fs::rename(&staging, output).map_err(|e| package_error(output, e))
}

/// An [`Error::Package`] about `path`.
fn package_error(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::Package {
        path: path.to_owned(),
        message: error.to_string(),
    }
}

/// `error`, an [`Error::InvalidKit`] named after `kit` when it is one.
fn renamed(error: Error, kit: &Path) -> Error {
    match error {
        Error::InvalidKit { message, .. } => Error::InvalidKit {
            kit: kit.display().to_string(),
            message,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixins_are_named_after_the_last_part_of_their_reference() {
        let descriptor = KitDescriptor::mixin("Acme");
        let mixin = Mixin::new("/kits/acme-tools/", descriptor.clone()).unwrap();
        assert_eq!(mixin.name(), "acme-tools");
        assert_eq!(mixin.reference(), "/kits/acme-tools/");
        assert_eq!(mixin.descriptor(), &descriptor);
        assert!(matches!(
            Mixin::new("/", descriptor),
            Err(Error::InvalidKit { .. })
        ));
    }

    #[test]
    fn invalid_descriptors_are_named_after_their_mixin() {
        let error = renamed(
            Error::InvalidKit {
                kit: "descriptor".to_owned(),
                message: "m".to_owned(),
            },
            Path::new("acme"),
        );
        assert!(matches!(error, Error::InvalidKit { ref kit, .. } if kit == "acme"));
        assert!(matches!(
            renamed(Error::Forge("f".to_owned()), Path::new("acme")),
            Error::Forge(_)
        ));
    }

    #[test]
    fn mixins_are_read_from_their_directory() {
        let dir = tempfile::tempdir().unwrap();
        let kit = dir.path().join("acme-tools");
        std::fs::create_dir(&kit).unwrap();
        let spec = KitDescriptor::mixin("Acme").to_spec();
        std::fs::write(kit.join("spec.yaml"), &spec).unwrap();
        let mixin = Mixin::read(&kit).unwrap();
        assert_eq!(mixin.name(), "acme-tools");
        assert_eq!(mixin.spec, spec);
        assert_eq!(
            mixin.reference(),
            kit.canonicalize().unwrap().display().to_string()
        );

        std::fs::write(kit.join("spec.yaml"), "{\"schemaVersion\": \"2\"}").unwrap();
        assert!(matches!(
            Mixin::read(&kit),
            Err(Error::InvalidKit { ref kit, .. }) if kit.ends_with("acme-tools")
        ));
        std::fs::remove_file(kit.join("spec.yaml")).unwrap();
        assert!(matches!(Mixin::read(&kit), Err(Error::File { .. })));
        assert!(matches!(
            Mixin::read(dir.path().join("none")),
            Err(Error::File { .. })
        ));
    }

    #[test]
    fn packages_are_written_through_a_staging_directory() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("pkg");
        std::fs::create_dir_all(output.join("old")).unwrap();
        write_package(&output, &json!({ "name": "{{name}}" }), &json!({}), &[b"a"]).unwrap();
        assert!(output.join("images/00.oci.tar").is_file());
        assert!(
            !output.join("old").exists(),
            "the previous package is replaced"
        );
        assert!(!output.with_extension("staging").exists());

        let blocked = dir.path().join("file");
        std::fs::write(&blocked, "x").unwrap();
        assert!(matches!(
            write_package(&blocked.join("pkg"), &json!({}), &json!({}), &[]),
            Err(Error::Package { .. })
        ));
    }
}
