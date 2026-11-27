//! Custom kits: kit packages, compiled with the `sbx` CLI (feature
//! `kit-compiler`) or forged without it (feature `kit-forge`).
//!
//! `sandboxd` cannot resolve kit references by itself: the `sbx` CLI
//! assembles the `kit_artifacts` of a creation request and loads local kit
//! sources into the daemon as OCI images. A *kit package* captures that output
//! once, so that sandboxes can then be created through the socket alone:
//!
//! ```text
//! <package>/kit.json            metadata, including the sbx version it was compiled with
//! <package>/create-body.json    creation request template, "name": "{{name}}"
//! <package>/images/NN.oci.tar   kit source images, loaded before creation
//! ```
//!
//! Packages are produced by `KitCompiler` (feature `kit-compiler`) or by
//! `audit kit-compile`; a package is tied to the `sbx` version that compiled
//! it, recompile it after upgrading `sbx`. `KitForge` (feature `kit-forge`)
//! writes them without the CLI, for mixins described by a `KitDescriptor`; a
//! forged package is tied to the API of the daemon it was forged for.

mod agent;
#[cfg(feature = "kit-compiler")]
mod compiler;
#[cfg(feature = "kit-forge")]
mod forge;
mod package;

pub use self::agent::KitAgent;
#[cfg(feature = "kit-compiler")]
#[cfg_attr(docsrs, doc(cfg(feature = "kit-compiler")))]
pub use self::compiler::{CompileJob, KitCompiler};
#[cfg(feature = "kit-forge")]
#[cfg_attr(docsrs, doc(cfg(feature = "kit-forge")))]
pub use self::forge::{KitDescriptor, KitForge, LifecycleStep, Mixin};
pub use self::package::{KitPackage, PackageImage};
