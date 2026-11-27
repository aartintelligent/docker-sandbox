//! Forging kit packages without the `sbx` CLI (feature `kit-forge`).
//!
//! A [`KitPackage`] holds what the daemon needs to create a sandbox from a
//! kit composition: a creation template whose `kit_artifacts` describe each
//! kit, and the source images of the local mixins. `KitCompiler` (feature
//! `kit-compiler`) captures them from the CLI; [`KitForge`] writes them
//! itself, through the socket alone: the base kit rebuilt from its published
//! image, or taken from a package compiled before, and mixins described by a
//! [`KitDescriptor`]:
//!
//! ```text
//! <output>/kit.json            metadata: forged by this crate, API of the daemon, kits, images
//! <output>/create-body.json    creation request template, "name": "{{name}}"
//! <output>/images/NN.oci.tar   source images of the mixins
//! ```
//!
//! A published base kit is pulled and exported once, when the package is
//! forged; its layers are read as they go by, never held in memory. The
//! daemon pulls the image again, when missing, as it creates a sandbox.
//!
//! The format of the artifacts and images is the private contract between the
//! CLI and the daemon, measured by the audit of this repository for the `sbx`
//! versions this crate supports ([`SUPPORTED_API`](crate::SUPPORTED_API)).
//! A forged package records the API it was forged for.
//!
//! The CLI checks the kit policy of the daemon before it uses a kit; forging
//! does too. It refuses when `kit.requireSignature` is on, since it cannot
//! check a signature and a forged kit carries none; mixins when
//! `kit.allowLocalKits` is off; a published base kit outside the sources of
//! `kit.allowedSources`. A setting the daemon does not know keeps its
//! default.

mod args;
mod artifact;
mod base;
mod descriptor;
mod export;
mod oci;

pub use self::descriptor::{KitDescriptor, LifecycleStep};

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use self::artifact::MixinParts;
use super::KitPackage;
use crate::client::Client;
use crate::error::{Error, Result};
use crate::sandbox::CreateRequest;

/// Setting of the daemon that allows kits from local directories.
const ALLOW_LOCAL_KITS: &str = "kit.allowLocalKits";

/// Setting of the daemon that requires signed kits.
const REQUIRE_SIGNATURE: &str = "kit.requireSignature";

/// Setting of the daemon listing the prefixes of the references kits may
/// come from, `*` for any.
const ALLOWED_SOURCES: &str = "kit.allowedSources";

/// Default of [`ALLOWED_SOURCES`], as the daemon describes it.
const DEFAULT_SOURCES: &[&str] = &["docker.io/", "dhi.io/"];

/// Name of the sandbox in the creation template of a package, replaced when
/// a sandbox is created from it.
const NAME_PLACEHOLDER: &str = "{{name}}";

/// What wrote a kit or a package: this crate, at its version.
#[derive(Debug, Serialize)]
struct Producer {
    /// Name of the crate.
    name: &'static str,
    /// Version of the crate.
    version: &'static str,
}

/// The metadata of a forged package, its `kit.json`.
#[derive(Debug, Serialize)]
struct Metadata {
    /// What forged it.
    forged_by: Producer,
    /// The daemon it was forged for.
    compiled_with: CompiledWith,
    /// Reference of the base kit.
    base: String,
    /// The published reference the base kit was rebuilt from, if it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    base_image: Option<String>,
    /// References of the mixins.
    mixins: Vec<String>,
    /// The source images of the mixins.
    images: Vec<ImageEntry>,
}

/// The daemon a package was forged for.
#[derive(Debug, Serialize)]
struct CompiledWith {
    /// Its API version, which `KitPackage` checks when it creates a sandbox.
    api_version: String,
}

/// A source image of a package.
#[derive(Debug, Serialize)]
struct ImageEntry {
    /// Path of its archive in the package.
    file: String,
    /// Name the daemon stores it under.
    #[serde(rename = "ref")]
    reference: String,
    /// Digest of its manifest.
    manifest_digest: String,
    /// Size of its archive, in bytes.
    bytes: usize,
}

impl Producer {
    /// This crate.
    fn this_crate() -> Self {
        Self {
            name: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// `value` as JSON. Serializing the types of this module cannot fail: their
/// maps have string keys.
fn to_json(value: &impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_default()
}

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
    /// read, and [`Error::InvalidKit`] when the directory is the root, which
    /// names no mixin, or the descriptor cannot be forged.
    pub fn read(dir: impl AsRef<Path>) -> Result<Self> {
        Self::read_dir(dir.as_ref())
    }

    /// [`Mixin::read`], compiled once whatever the type of path.
    fn read_dir(dir: &Path) -> Result<Self> {
        let file_error = |path: &Path| {
            let path = path.to_owned();
            move |source| Error::File { path, source }
        };
        let canonical = dir.canonicalize().map_err(file_error(dir))?;
        let reference = canonical.display().to_string();
        let name = mixin_name(&reference)?;
        let spec_path = canonical.join("spec.yaml");
        let spec = std::fs::read_to_string(&spec_path).map_err(file_error(&spec_path))?;
        let descriptor = KitDescriptor::parse(&spec).map_err(|error| renamed(error, dir))?;
        Ok(Self {
            reference,
            name,
            descriptor,
            spec,
        })
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
        let name = mixin_name(&reference)?;
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

/// Where the base kit of a package comes from.
#[derive(Debug, Clone)]
enum Base {
    /// A published kit, rebuilt from its image.
    Published(String),
    /// The first kit of a compiled package.
    Package(KitPackage),
}

/// The base kit of a package being forged.
struct ResolvedBase {
    /// Creation template, named `{{name}}`, without `kits` and
    /// `kit_artifacts`.
    request: CreateRequest,
    /// Reference of the kit, the first entry of `kits`.
    reference: String,
    /// The kit artifact, the first entry of `kit_artifacts`.
    artifact: Value,
    /// The published reference it was rebuilt from, if it was.
    image: Option<String>,
}

/// Writes kit packages without the `sbx` CLI (feature `kit-forge`): a base
/// kit, rebuilt from its published image or taken from a compiled package,
/// and mixins forged from their descriptor.
///
/// ```no_run
/// use docker_sandbox::Client;
/// use docker_sandbox::kit::{KitDescriptor, KitForge, LifecycleStep, Mixin};
///
/// # async fn run() -> docker_sandbox::Result<()> {
/// let client = Client::connect().await?;
/// let tools = KitDescriptor::mixin("Acme tools")
///     .with_network_allow(["api.acme.example:443"])
///     .with_install(LifecycleStep::new("pip install acme-cli").as_user("agent"));
/// let package = KitForge::new(client.clone())
///     .with_base("docker.io/sbx/claude:latest")
///     .with_arg("model", "claude-haiku-5-5")
///     .with_mixin(Mixin::new("/kits/acme-tools", tools)?)
///     .forge("/var/lib/kits/claude+acme-tools")
///     .await?;
/// package.create(&client, "job-42").await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct KitForge {
    /// Client of the daemon the package is forged for.
    client: Client,
    /// The base kit the mixins are composed with.
    base: Option<Base>,
    /// Values of the arguments of a published base kit, by name.
    args: BTreeMap<String, String>,
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
            args: BTreeMap::new(),
            mixins: Vec::new(),
        }
    }

    /// Takes as base kit the published v3 workload kit `reference`, e.g.
    /// `docker.io/sbx/claude:latest`, optionally pinned by digest as
    /// `docker.io/sbx/claude:latest@sha256:…`, and normalized as Docker does
    /// (`sbx/shell` is `docker.io/sbx/shell:latest`). Its image is pulled and
    /// exported when the package is forged, and the kit rebuilt from it.
    #[must_use]
    pub fn with_base(mut self, reference: impl Into<String>) -> Self {
        self.base = Some(Base::Published(reference.into()));
        self
    }

    /// Takes the base kit, the first kit, of `package`: compiled once with
    /// `KitCompiler` (feature `kit-compiler`) or `audit kit-compile`. Its
    /// other kits and their images are left out.
    #[must_use]
    pub fn with_base_package(mut self, package: KitPackage) -> Self {
        self.base = Some(Base::Package(package));
        self
    }

    /// Gives the argument `name` of the published base kit the value `value`,
    /// in place of its default. The value is checked against the values or
    /// the pattern the kit declares, substituted for its
    /// `${{ kit.args.<name> }}` placeholders, and set in the variable the kit
    /// names, as the CLI does with `--kit-arg`.
    #[must_use]
    pub fn with_arg(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.args.insert(name.into(), value.into());
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
    /// Returns [`Error::Forge`] without a base kit, with a compiled package
    /// but no mixin or with arguments, when two kits share a name, when the
    /// kit policy of the daemon refuses the kits, or when a pinned base kit
    /// exports another digest; [`Error::InvalidKit`] when the base kit is not
    /// a v3 workload kit or refuses its arguments; the errors of the settings,
    /// pull and export requests; [`Error::Archive`] and [`Error::Package`]
    /// when the export cannot be read or the package cannot be written.
    pub async fn forge(&self, output: impl Into<PathBuf>) -> Result<KitPackage> {
        self.forge_to(output.into()).await
    }

    /// [`KitForge::forge`], compiled once whatever the type of path.
    async fn forge_to(&self, output: PathBuf) -> Result<KitPackage> {
        let base = self.resolve_base().await?;
        let base_name = base
            .artifact
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

        let mut kits = vec![base.reference.clone()];
        let mut artifacts = vec![base.artifact];
        let mut images = Vec::new();
        for mixin in &self.mixins {
            let image = oci::source_image(&mixin.spec, &mixin.descriptor);
            let artifact = artifact::mixin(&MixinParts {
                name: &mixin.name,
                reference: &mixin.reference,
                descriptor: &mixin.descriptor,
                image: &image.reference,
                manifest_digest: &image.manifest_digest,
            });
            artifacts.push(artifact.into_json());
            kits.push(mixin.reference.clone());
            images.push(image);
        }

        let metadata = Metadata {
            forged_by: Producer::this_crate(),
            compiled_with: CompiledWith {
                api_version: self.client.health().api_version.clone(),
            },
            base: base.reference,
            base_image: base.image,
            mixins: kits[1..].to_vec(),
            images: images
                .iter()
                .enumerate()
                .map(|(index, image)| ImageEntry {
                    file: image_file(index),
                    reference: image.reference.clone(),
                    manifest_digest: image.manifest_digest.clone(),
                    bytes: image.archive.len(),
                })
                .collect(),
        };
        let mut request = base.request;
        request.kits = kits;
        request.kit_artifacts = artifacts;
        let archives: Vec<&[u8]> = images.iter().map(|i| i.archive.as_slice()).collect();
        write_package(&output, &to_json(&request), &to_json(&metadata), &archives)?;
        KitPackage::open(&output)
    }

    /// The base kit, once the kit policy of the daemon is checked: taken from
    /// its package, or rebuilt from its image.
    async fn resolve_base(&self) -> Result<ResolvedBase> {
        match &self.base {
            None => Err(Error::Forge(
                "no base kit: give a published kit with `with_base` or a compiled package with \
                 `with_base_package`"
                    .to_owned(),
            )),
            Some(Base::Package(package)) => {
                if !self.args.is_empty() {
                    return Err(Error::Forge(
                        "kit arguments apply to a published base kit: a compiled package holds \
                         the values it was compiled with"
                            .to_owned(),
                    ));
                }
                if self.mixins.is_empty() {
                    return Err(Error::Forge(
                        "no mixin to forge on the compiled package".to_owned(),
                    ));
                }
                self.check_policy(None).await?;
                Ok(ResolvedBase {
                    request: package.request(NAME_PLACEHOLDER)?,
                    reference: package.kit_refs().into_iter().next().unwrap_or_default(),
                    artifact: package
                        .kit_artifacts()
                        .into_iter()
                        .next()
                        .unwrap_or_default(),
                    image: None,
                })
            }
            Some(Base::Published(requested)) => {
                let reference = base::Reference::parse(requested)?;
                self.check_policy(Some(&reference)).await?;
                let kit = base::rebuild(&self.client, &reference, &self.args).await?;
                let mut request = CreateRequest::for_agent(NAME_PLACEHOLDER, kit.name);
                if !self.args.is_empty() {
                    request
                        .extra
                        .insert("kit_args".to_owned(), json!(self.args));
                }
                Ok(ResolvedBase {
                    request,
                    reference: kit.reference,
                    artifact: kit.artifact.into_json(),
                    image: Some(requested.clone()),
                })
            }
        }
    }

    /// Refuses to forge when the kit policy of the daemon requires signed
    /// kits, forbids local kits and there are mixins, or does not allow the
    /// source of the published base kit `published`.
    async fn check_policy(&self, published: Option<&base::Reference>) -> Result<()> {
        if self.setting(REQUIRE_SIGNATURE).await? == Some(Value::Bool(true)) {
            return Err(Error::Forge(format!(
                "the daemon requires signed kits ({REQUIRE_SIGNATURE} is on), and forging can \
                 neither check nor give a signature"
            )));
        }
        if !self.mixins.is_empty()
            && self.setting(ALLOW_LOCAL_KITS).await? == Some(Value::Bool(false))
        {
            return Err(Error::Forge(format!(
                "the daemon forbids local kits ({ALLOW_LOCAL_KITS} is off)"
            )));
        }
        let Some(reference) = published else {
            return Ok(());
        };
        let sources: Vec<String> = match self.setting(ALLOWED_SOURCES).await? {
            None => DEFAULT_SOURCES.iter().map(|&s| s.to_owned()).collect(),
            Some(value) => serde_json::from_value(value).map_err(|_| {
                Error::Forge(format!(
                    "the daemon holds a {ALLOWED_SOURCES} that is not a list of prefixes"
                ))
            })?,
        };
        let repository = &reference.repository;
        if sources
            .iter()
            .any(|prefix| prefix == "*" || repository.starts_with(prefix.as_str()))
        {
            return Ok(());
        }
        Err(Error::Forge(format!(
            "the daemon only allows kits from {} ({ALLOWED_SOURCES}), not {repository}",
            sources.join(", ")
        )))
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

/// The name of the mixin listed under `reference`: its last part.
///
/// # Errors
///
/// Returns [`Error::InvalidKit`] when the reference names nothing, as `/`.
fn mixin_name(reference: &str) -> Result<String> {
    let name = reference
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if name.is_empty() {
        return Err(Error::InvalidKit {
            kit: reference.to_owned(),
            message: "the reference of a mixin is a path that names it".to_owned(),
        });
    }
    Ok(name.to_owned())
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
    let failed = |error: std::io::Error| package_error(&staging, &error);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(staging.join("images")).map_err(failed)?;
    // Serializing a `Value` cannot fail: its map keys are strings.
    let pretty = |value: &Value| serde_json::to_vec_pretty(value).unwrap_or_default();
    let files = archives
        .iter()
        .enumerate()
        .map(|(index, archive)| (image_file(index), archive.to_vec()))
        .chain([
            ("create-body.json".to_owned(), pretty(template)),
            ("kit.json".to_owned(), pretty(metadata)),
        ]);
    for (file, bytes) in files {
        std::fs::write(staging.join(file), bytes).map_err(failed)?;
    }
    let _ = std::fs::remove_dir_all(output);
    std::fs::rename(&staging, output).map_err(|error| package_error(output, &error))
}

/// An [`Error::Package`] about `path`.
fn package_error(path: &Path, error: &std::io::Error) -> Error {
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
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn mixins_are_named_after_the_last_part_of_their_reference() {
        let descriptor = KitDescriptor::mixin("Acme");
        let mixin = Mixin::new("/kits/acme-tools/", descriptor.clone()).unwrap();
        assert_eq!(mixin.name(), "acme-tools");
        assert_eq!(mixin.reference(), "/kits/acme-tools/");
        assert_eq!(mixin.descriptor(), &descriptor);
        let error = Mixin::new("/", descriptor).unwrap_err().to_string();
        assert_eq!(
            error,
            "invalid kit /: the reference of a mixin is a path that names it"
        );
        let empty_step = KitDescriptor::mixin("Acme").with_install(LifecycleStep::new(" "));
        let error = Mixin::new("/kits/acme-tools", empty_step)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("invalid kit acme-tools: capabilities[0]: install[0]: no command"),
            "{error}"
        );
    }

    #[test]
    fn invalid_descriptors_are_named_after_their_mixin() {
        let invalid = Error::InvalidKit {
            kit: "descriptor".to_owned(),
            message: "m".to_owned(),
        };
        assert_eq!(
            renamed(invalid, Path::new("acme")).to_string(),
            "invalid kit acme: m"
        );
        let refused = renamed(Error::Forge("f".to_owned()), Path::new("acme"));
        assert_eq!(refused.to_string(), "kit forging refused: f");
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
        let error = Mixin::read(&kit).unwrap_err().to_string();
        assert!(
            error.starts_with(&format!("invalid kit {}:", kit.display())),
            "{error}"
        );
        std::fs::remove_file(kit.join("spec.yaml")).unwrap();
        let error = Mixin::read(&kit).unwrap_err().to_string();
        assert!(error.starts_with("cannot access "), "{error}");
        let error = Mixin::read(dir.path().join("none"))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("cannot access "), "{error}");
        let error = Mixin::read("/").unwrap_err().to_string();
        assert!(
            error.ends_with("the reference of a mixin is a path that names it"),
            "{error}"
        );
    }

    #[test]
    fn packages_are_written_through_a_staging_directory() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("pkg");
        std::fs::create_dir_all(output.join("old")).unwrap();
        write_package(&output, &json!({ "name": "{{name}}" }), &json!({}), &[b"a"]).unwrap();
        assert_eq!(
            std::fs::read(output.join("images/00.oci.tar")).unwrap(),
            b"a"
        );
        assert!(output.join("create-body.json").is_file() && output.join("kit.json").is_file());
        assert!(
            !output.join("old").exists(),
            "the previous package is replaced"
        );
        assert!(!output.with_extension("staging").exists());
    }

    #[test]
    fn packages_that_cannot_be_written_are_package_errors() {
        let dir = tempfile::tempdir().unwrap();
        let refusal = |output: &Path| {
            let error = write_package(output, &json!({}), &json!({}), &[]).unwrap_err();
            error.to_string()
        };

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
    }
}
