//! Forging kit packages without the `sbx` CLI (feature `forge`).
//!
//! A [`KitPackage`] holds what the daemon needs to create a sandbox from a
//! kit composition: a creation template whose `kit_artifacts` describe each
//! kit, and the source images of the local mixins. `KitCompiler` (feature
//! `compiler`) captures them from the CLI; [`KitForge`] writes them
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
//!
//! An agent is resolved as the CLI resolves `sbx create <agent>`: to the kit
//! the daemon names for it in `kit.shortcuts.<agent>`, when it names one, and
//! to its published kit otherwise.

mod args;
mod artifact;
mod base;
mod cache;
mod descriptor;
mod export;
mod mcp;
mod oci;

pub use self::cache::KitCache;
pub use self::descriptor::{KitDescriptor, LifecycleStep};
pub use self::mcp::McpServer;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

use self::artifact::MixinParts;
use crate::error::{Error, Result};
use crate::{Agent, KitRef};
use docker_sandbox::Client;
use docker_sandbox::kit::{KitPackage, PackageImage};
use docker_sandbox::sandbox::CreateRequest;

/// Setting of the daemon that allows kits from local directories.
const ALLOW_LOCAL_KITS: &str = "kit.allowLocalKits";

/// Setting of the daemon that requires signed kits.
const REQUIRE_SIGNATURE: &str = "kit.requireSignature";

/// Setting of the daemon listing the prefixes of the references kits may
/// come from, `*` for any.
const ALLOWED_SOURCES: &str = "kit.allowedSources";

/// Prefix of the settings of the daemon naming the kit an agent resolves to
/// instead of its published kit, e.g. `kit.shortcuts.claude`.
const SHORTCUTS: &str = "kit.shortcuts.";

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
/// Its JSON form is `{"reference": …, "descriptor": …}`, read and checked as
/// [`Mixin::new`] makes a mixin. The text of a `spec.yaml` it was read from
/// is not part of it: read again, the mixin writes its descriptor out.
///
/// ```
/// use docker_sandbox_kit::{KitDescriptor, LifecycleStep, Mixin};
///
/// let descriptor = KitDescriptor::mixin("Acme tools")
///     .with_install(LifecycleStep::new("pip install acme-cli").as_user("agent"));
/// let mixin = Mixin::new("/kits/acme-tools", descriptor)?;
/// assert_eq!(mixin.name(), "acme-tools");
/// # Ok::<(), docker_sandbox_kit::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Returns [`Error::Client`] ([`docker_sandbox::Error::File`]) when the directory or its `spec.yaml` cannot be
    /// read, and [`Error::InvalidKit`] when the directory is the root, which
    /// names no mixin, or the descriptor cannot be forged.
    pub fn read(dir: impl AsRef<Path>) -> Result<Self> {
        Self::read_dir(dir.as_ref())
    }

    /// Writes the mixin into the directory `dir`, created if needed, as the
    /// CLI reads a local kit: its `spec.yaml`, the descriptor in JSON behind
    /// its `# syntax=` line. Returns the mixin of that directory, named after
    /// it and listed under its canonical path, as the CLI lists the kit in
    /// the packages it compiles (`sbx create --kit <dir>`, `KitCompiler`).
    ///
    /// ```no_run
    /// use docker_sandbox_kit::{KitDescriptor, LifecycleStep, Mixin};
    ///
    /// let tools = KitDescriptor::mixin("Acme tools")
    ///     .with_install(LifecycleStep::new("pip install acme-cli").as_user("agent"));
    /// let saved = Mixin::new("/kits/acme-tools", tools)?.save("/var/lib/kits/acme-tools")?;
    /// assert_eq!(saved.name(), "acme-tools");
    /// # Ok::<(), docker_sandbox_kit::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Client`] ([`docker_sandbox::Error::File`]) when the
    /// directory or its `spec.yaml` cannot be written, and as
    /// [`Mixin::read`] when they cannot be read again.
    pub fn save(&self, dir: impl AsRef<Path>) -> Result<Self> {
        self.save_to(dir.as_ref())
    }

    /// [`Mixin::save`], compiled once whatever the type of path.
    fn save_to(&self, dir: &Path) -> Result<Self> {
        let file_error = |path: &Path| {
            let path = path.to_owned();
            move |source| docker_sandbox::Error::File { path, source }
        };
        std::fs::create_dir_all(dir).map_err(file_error(dir))?;
        let spec = dir.join("spec.yaml");
        std::fs::write(&spec, &self.spec).map_err(file_error(&spec))?;
        Self::read_dir(dir)
    }

    /// [`Mixin::read`], compiled once whatever the type of path.
    fn read_dir(dir: &Path) -> Result<Self> {
        let file_error = |path: &Path| {
            let path = path.to_owned();
            move |source| docker_sandbox::Error::File { path, source }
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

    /// The mixin `kit` names, read as the CLI reads a mixin
    /// ([`KitRef::mixin`]): a directory is read as [`Mixin::read`] reads it.
    ///
    /// # Errors
    ///
    /// As [`Mixin::read`] for a directory, and [`Error::InvalidKit`] for any
    /// other kit: an image, a ZIP file or a Git repository is resolved by the
    /// CLI, and an agent is a base kit.
    pub fn from_ref(kit: &KitRef) -> Result<Self> {
        let other = |kit: String, what: &str| Error::InvalidKit {
            kit,
            message: format!("{what}: a forged mixin is read from its directory"),
        };
        match kit {
            KitRef::Local(path) if path.is_dir() => Self::read(path),
            KitRef::Local(path) => Err(other(path.display().to_string(), "not a directory")),
            KitRef::Image(image) => Err(other(image.clone(), "an image the sbx CLI resolves")),
            KitRef::Git(repository) => Err(other(
                repository.clone(),
                "a Git repository the sbx CLI resolves",
            )),
            KitRef::Agent(agent) => Err(other(agent.to_string(), "an agent is a base kit")),
        }
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

/// The JSON form of a [`Mixin`]: its reference and its descriptor.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MixinForm {
    /// Reference of the mixin.
    reference: String,
    /// Its descriptor.
    descriptor: KitDescriptor,
}

impl Serialize for Mixin {
    /// Writes the mixin as its reference and its descriptor: the text of a
    /// `spec.yaml` it was read from is not kept, the descriptor is.
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        MixinForm {
            reference: self.reference.clone(),
            descriptor: self.descriptor.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Mixin {
    /// Reads a mixin as [`Mixin::new`] makes one, checked as it checks it.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let form = MixinForm::deserialize(deserializer)?;
        Self::new(form.reference, form.descriptor).map_err(serde::de::Error::custom)
    }
}

/// Where the base kit of a package comes from.
#[derive(Debug, Clone)]
enum Base {
    /// The kit of an agent: its published kit, or the one its shortcut on
    /// the daemon names.
    Agent(Agent),
    /// A published kit, rebuilt from its image.
    Published(String),
    /// A kit only the CLI builds, from a directory, a ZIP file or a Git
    /// repository, named as given.
    Cli(String),
    /// The first kit of a compiled package.
    Package(KitPackage),
}

/// The base kit of a package, checked before it is forged.
enum CheckedBase<'a> {
    /// A compiled package.
    Package(&'a KitPackage),
    /// A published kit.
    Published {
        /// Its reference, as given or as the daemon names it for an agent.
        requested: String,
        /// That reference, read.
        reference: base::Reference,
        /// Its arguments, by their name in the kit.
        args: BTreeMap<String, String>,
    },
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

/// Writes kit packages without the `sbx` CLI (feature `forge`): a base
/// kit, rebuilt from its published image or taken from a compiled package,
/// and mixins forged from their descriptor.
///
/// ```no_run
/// use docker_sandbox::Client;
/// use docker_sandbox_kit::{KitDescriptor, KitForge, LifecycleStep, Mixin};
///
/// # async fn run() -> docker_sandbox_kit::Result<()> {
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
    /// The client of the daemon the package is forged for.
    pub(super) fn client(&self) -> &Client {
        &self.client
    }

    /// The key of the composition: a digest of its base kit, its arguments
    /// and its mixins, the text of their descriptors included.
    pub(super) fn composition_key(&self) -> String {
        composition_key(self.base.as_ref(), &self.args, &self.mixins)
    }

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

    /// Forges on the kit of `agent`, as the CLI resolves `sbx create
    /// <name>`: the kit the daemon names in `kit.shortcuts.<name>` when it
    /// names one, its published kit [`Agent::reference`]
    /// (`docker.io/sbx/<name>:latest`) otherwise, then as
    /// [`KitForge::with_base`] takes it.
    #[must_use]
    pub fn agent(client: Client, agent: Agent) -> Self {
        Self {
            base: Some(Base::Agent(agent)),
            ..Self::new(client)
        }
    }

    /// Takes as base kit the published v3 workload kit `reference`, e.g.
    /// `docker.io/sbx/claude:latest`, optionally pinned by digest as
    /// `docker.io/sbx/claude:latest@sha256:…` or prefixed with `oci://`, and
    /// normalized as Docker does (`sbx/shell` is
    /// `docker.io/sbx/shell:latest`). Its image is pulled and exported when
    /// the package is forged, and the kit rebuilt from it, its source
    /// recorded as `reference` is written, as the CLI records it. A kit to
    /// build from a directory or a Git repository is refused: the CLI builds
    /// it.
    #[must_use]
    pub fn with_base(mut self, reference: impl Into<String>) -> Self {
        self.base = Some(Base::Published(reference.into()));
        self
    }

    /// Takes as base kit `kit`, read as the CLI reads a base kit
    /// ([`KitRef::base`]): an agent as [`KitForge::agent`] takes it, an image
    /// as [`KitForge::with_base`] does. A directory, a ZIP file or a Git
    /// repository, which the CLI builds, fails the forging.
    #[must_use]
    pub fn with_kit(mut self, kit: KitRef) -> Self {
        self.base = Some(match kit {
            KitRef::Agent(agent) => Base::Agent(agent),
            KitRef::Image(reference) => Base::Published(reference),
            KitRef::Local(path) => Base::Cli(path.display().to_string()),
            KitRef::Git(repository) => Base::Cli(repository),
        });
        self
    }

    /// Takes the base kit, the first kit, of `package`: compiled once with
    /// `KitCompiler` (feature `compiler`) or `audit kit-compile`. Its
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
    /// names, as the CLI does with `--kit-arg`. As the CLI, `name` may name
    /// its kit, `claude.model` for the base kit `claude`; a mixin declares
    /// no argument.
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

    /// Forges the package ([`KitForge::build`]) and saves it to `output`,
    /// replacing a package already there ([`KitPackage::save`]).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Forge`] without a base kit, with a compiled package
    /// but no mixin or with arguments, when two kits share a name, when the
    /// kit policy of the daemon refuses the kits, or when a pinned base kit
    /// exports another digest; [`Error::InvalidKit`] when the base kit is not
    /// a v3 workload kit or refuses its arguments; the errors of the settings,
    /// pull and export requests; [`Error::Client`] ([`docker_sandbox::Error::Archive`]) and [`Error::Client`] ([`docker_sandbox::Error::Package`])
    /// when the export cannot be read or the package cannot be written.
    pub async fn forge(&self, output: impl Into<PathBuf>) -> Result<KitPackage> {
        self.forge_to(output.into()).await
    }

    /// [`KitForge::forge`], compiled once whatever the type of path.
    async fn forge_to(&self, output: PathBuf) -> Result<KitPackage> {
        self.build().await?.save(output).map_err(Error::from)
    }

    /// Forges the package in memory: nothing is written. The package creates
    /// sandboxes as it is, and [`KitPackage::save`] writes it to a directory
    /// when it is to be kept.
    ///
    /// ```no_run
    /// # async fn run(client: docker_sandbox::Client) -> docker_sandbox_kit::Result<()> {
    /// use docker_sandbox_kit::{Agent, KitForge};
    ///
    /// let package = KitForge::agent(client.clone(), Agent::Shell).build().await?;
    /// package.create(&client, "job-42").await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`KitForge::forge`], but for the writing of the package.
    pub async fn build(&self) -> Result<KitPackage> {
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
        let images = images
            .into_iter()
            .map(|image| PackageImage::in_memory(image.reference, image.archive));
        KitPackage::in_memory(to_json(&request), to_json(&metadata), images).map_err(Error::from)
    }

    /// Checks what can be checked before forging, without pulling or writing
    /// anything: a base kit is named, an agent resolves to a kit forging
    /// takes, the arguments name the base kit, and the kit policy of the
    /// daemon accepts the composition. [`KitForge::build`] checks the same
    /// first; the arguments are checked against the descriptor of the base
    /// kit only then, as it comes with its image.
    ///
    /// ```no_run
    /// # async fn run(client: docker_sandbox::Client) -> docker_sandbox_kit::Result<()> {
    /// use docker_sandbox_kit::{Agent, KitForge};
    ///
    /// let forge = KitForge::agent(client, Agent::Claude);
    /// // Refused before any image is pulled when the daemon requires signed kits.
    /// forge.check().await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Forge`] when no base kit is named, the base kit is one
    /// only the CLI builds, a compiled base package is given arguments or no
    /// mixin, or the kit policy of the daemon refuses the composition;
    /// [`Error::InvalidKit`] when an argument names a kit that is not the base
    /// kit, or is given twice, or when the reference of the base kit cannot be
    /// read; and [`Error::Client`] when the settings of the daemon cannot be
    /// read.
    pub async fn check(&self) -> Result<()> {
        self.checked_base().await.map(drop)
    }

    /// The base kit, once the kit policy of the daemon is checked, as
    /// [`KitForge::check`] checks it.
    async fn checked_base(&self) -> Result<CheckedBase<'_>> {
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
                Ok(CheckedBase::Package(package))
            }
            Some(Base::Agent(agent)) => {
                let requested = self.agent_kit(*agent).await?;
                self.checked_published(requested).await
            }
            Some(Base::Published(requested)) => self.checked_published(requested.clone()).await,
            Some(Base::Cli(kit)) => Err(Error::Forge(format!(
                "{kit} is a kit the sbx CLI builds, from a directory, a ZIP file or a Git \
                 repository: compile it with KitCompiler, or take a published kit"
            ))),
        }
    }

    /// The published base kit `requested`, once its reference is read, the
    /// kit policy of the daemon checked and its arguments named.
    async fn checked_published(&self, requested: String) -> Result<CheckedBase<'_>> {
        let reference = base::Reference::parse(&requested)?;
        self.check_policy(Some(&reference)).await?;
        let args = self.base_args(reference.name())?;
        Ok(CheckedBase::Published {
            requested,
            reference,
            args,
        })
    }

    /// The base kit, once it is checked: taken from its package, or rebuilt
    /// from its image.
    async fn resolve_base(&self) -> Result<ResolvedBase> {
        match self.checked_base().await? {
            CheckedBase::Package(package) => Ok(ResolvedBase {
                request: package.request(NAME_PLACEHOLDER)?,
                reference: package.kit_refs().into_iter().next().unwrap_or_default(),
                artifact: package
                    .kit_artifacts()
                    .into_iter()
                    .next()
                    .unwrap_or_default(),
                image: None,
            }),
            CheckedBase::Published {
                requested,
                reference,
                args,
            } => {
                let kit = base::rebuild(&self.client, &reference, &args).await?;
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
                    image: Some(requested),
                })
            }
        }
    }

    /// The arguments of the base kit `base`, from the values given: one named
    /// `<kit>.<name>`, as the CLI takes it, is the argument `name` of the kit
    /// `kit` of the composition, and the mixins declare none.
    fn base_args(&self, base: &str) -> Result<BTreeMap<String, String>> {
        let invalid = |kit: &str, message: String| Error::InvalidKit {
            kit: kit.to_owned(),
            message,
        };
        let mut args = BTreeMap::new();
        for (given, value) in &self.args {
            let name = match given.split_once('.') {
                None => given.as_str(),
                Some((kit, name)) if kit == base => name,
                Some((kit, name)) if self.mixins.iter().any(|mixin| mixin.name == kit) => {
                    return Err(invalid(
                        kit,
                        format!("the kit declares no argument {name}: a mixin declares none"),
                    ));
                }
                Some((kit, _)) => {
                    let mut kits: Vec<&str> = self.mixins.iter().map(|m| m.name.as_str()).collect();
                    kits.push(base);
                    kits.sort_unstable();
                    return Err(invalid(
                        base,
                        format!(
                            "no kit named {kit} is composed (kits: {}), for the argument {given}",
                            kits.join(", ")
                        ),
                    ));
                }
            };
            if args.insert(name.to_owned(), value.clone()).is_some() {
                return Err(invalid(
                    base,
                    format!("the argument {name} is given twice, as {name} and {base}.{name}"),
                ));
            }
        }
        Ok(args)
    }

    /// The kit `agent` resolves to: the one its shortcut on the daemon
    /// names, or its published kit.
    async fn agent_kit(&self, agent: Agent) -> Result<String> {
        let key = format!("{SHORTCUTS}{agent}");
        match self.setting(&key).await? {
            Some(Value::String(kit)) if !kit.is_empty() => {
                if kit.starts_with("git+") || kit.starts_with('/') {
                    return Err(Error::Forge(format!(
                        "the daemon resolves {agent} to {kit} ({key}), a kit only the sbx CLI \
                         builds"
                    )));
                }
                Ok(kit)
            }
            _ => Ok(agent.reference()),
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
            Err(error) => Err(error.into()),
        }
    }
}

/// The key of the composition of `base`, `args` and `mixins`: the first 16
/// hexadecimal digits of a SHA-256 digest of what they are.
fn composition_key(
    base: Option<&Base>,
    args: &BTreeMap<String, String>,
    mixins: &[Mixin],
) -> String {
    let base = match base {
        None => Value::Null,
        Some(Base::Agent(agent)) => json!({ "agent": agent.name() }),
        Some(Base::Published(reference)) => json!({ "image": reference }),
        Some(Base::Cli(kit)) => json!({ "cli": kit }),
        Some(Base::Package(package)) => {
            json!({ "package": package.kit_refs().first(), "artifact": package.kit_artifacts().first() })
        }
    };
    let mixins: Vec<Value> = mixins
        .iter()
        .map(|mixin| json!({ "reference": mixin.reference, "spec": mixin.spec }))
        .collect();
    let composition = json!({ "base": base, "args": args, "mixins": mixins });
    let digest = oci::digest(composition.to_string().as_bytes());
    digest["sha256:".len().."sha256:".len() + 16].to_owned()
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
    fn mixins_are_saved_as_the_cli_reads_them() {
        let dir = tempfile::tempdir().unwrap();
        let descriptor = KitDescriptor::mixin("Acme")
            .with_install(LifecycleStep::new("make tools").as_user("agent"));
        let mixin = Mixin::new("/kits/acme-tools", descriptor).unwrap();
        let output = dir.path().join("kits/acme-tools");
        let saved = mixin.save(&output).unwrap();
        assert_eq!(saved.name(), "acme-tools");
        assert_eq!(saved.descriptor(), mixin.descriptor());
        assert_eq!(
            saved.reference(),
            output.canonicalize().unwrap().display().to_string()
        );
        assert_eq!(
            std::fs::read_to_string(output.join("spec.yaml")).unwrap(),
            mixin.spec
        );
        assert_eq!(Mixin::read(&output).unwrap(), saved);

        // Under a file, nothing can be written.
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let error = mixin.save(file.join("acme")).unwrap_err().to_string();
        assert!(error.starts_with("cannot access "), "{error}");
        // A directory named `spec.yaml` takes no file of that name.
        std::fs::create_dir_all(dir.path().join("taken/spec.yaml")).unwrap();
        let error = mixin
            .save(dir.path().join("taken"))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("cannot access "), "{error}");
    }

    #[test]
    fn mixins_are_taken_from_a_kit_only_when_it_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let kit = dir.path().join("acme-tools");
        std::fs::create_dir(&kit).unwrap();
        std::fs::write(
            kit.join("spec.yaml"),
            KitDescriptor::mixin("Acme").to_spec(),
        )
        .unwrap();
        let mixin = Mixin::from_ref(&KitRef::Local(kit.clone())).unwrap();
        assert_eq!(mixin, Mixin::read(&kit).unwrap());
        let file = kit.join("spec.yaml");
        for (other, expected) in [
            (KitRef::Local(file), "not a directory"),
            (
                KitRef::Image("ghcr.io/acme/tools:1".into()),
                "an image the sbx CLI resolves",
            ),
            (
                KitRef::Git("git+https://host/r.git".into()),
                "a Git repository the sbx CLI resolves",
            ),
            (KitRef::Agent(Agent::Claude), "an agent is a base kit"),
        ] {
            let error = Mixin::from_ref(&other).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn compositions_are_keyed_by_what_they_are() {
        let mixin = |text: &str| {
            Mixin::new(
                "/kits/acme",
                KitDescriptor::mixin("Acme").with_description(text),
            )
            .unwrap()
        };
        let none = BTreeMap::new();
        let args = BTreeMap::from([("model".to_owned(), "m".to_owned())]);
        let package =
            KitPackage::in_memory(serde_json::json!({ "name": "{{name}}" }), Value::Null, [])
                .unwrap();
        let bases = [
            None,
            Some(Base::Agent(Agent::Shell)),
            Some(Base::Published("docker.io/sbx/shell:latest".into())),
            Some(Base::Cli("./kit".into())),
            Some(Base::Package(package)),
        ];
        let mut keys: Vec<String> = bases
            .iter()
            .map(|base| composition_key(base.as_ref(), &none, &[]))
            .collect();
        let agent = bases[1].as_ref();
        keys.push(composition_key(agent, &args, &[]));
        keys.push(composition_key(agent, &none, &[mixin("one")]));
        // The same mixin with another descriptor is another composition.
        keys.push(composition_key(agent, &none, &[mixin("two")]));
        assert!(keys.iter().all(|key| key.len() == 16));
        let distinct: BTreeSet<&String> = keys.iter().collect();
        assert_eq!(distinct.len(), keys.len(), "{keys:?}");
        assert_eq!(composition_key(agent, &none, &[mixin("one")]), keys[6]);
    }

    #[test]
    fn descriptors_and_mixins_are_written_and_read_as_json() {
        let descriptor = KitDescriptor::mixin("Acme")
            .with_install(LifecycleStep::new("make tools").as_user("agent"))
            .with_network_deny(["*.example.com"]);
        let json = serde_json::to_value(&descriptor).unwrap();
        assert_eq!(json, descriptor.to_json());
        assert_eq!(
            serde_json::from_value::<KitDescriptor>(json).unwrap(),
            descriptor
        );
        let refused =
            serde_json::from_value::<KitDescriptor>(serde_json::json!({ "schemaVersion": "2" }));
        assert!(refused.unwrap_err().to_string().contains("invalid kit"));
        assert!(serde_json::from_str::<KitDescriptor>("{").is_err());

        let mixin = Mixin::new("/kits/acme", descriptor.clone()).unwrap();
        let json = serde_json::to_value(&mixin).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "reference": "/kits/acme", "descriptor": descriptor.to_json() })
        );
        assert_eq!(serde_json::from_value::<Mixin>(json).unwrap(), mixin);
        for refused in [
            serde_json::json!({ "reference": "/", "descriptor": descriptor.to_json() }),
            serde_json::json!({ "reference": "/kits/acme", "descriptor": descriptor.to_json(), "dir": "x" }),
        ] {
            assert!(serde_json::from_value::<Mixin>(refused).is_err());
        }
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
}
