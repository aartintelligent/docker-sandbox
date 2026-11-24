//! Custom kits: compiled kit packages, and their compilation with the `sbx`
//! CLI (feature `kit-compiler`).
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
//! Packages are produced by [`KitCompiler`] (feature `kit-compiler`) or by
//! `audit kit-compile`. A package is tied to the `sbx` version that compiled
//! it; recompile it after upgrading `sbx`.

mod agent;
#[cfg(feature = "kit-compiler")]
mod compiler;
mod package;

pub use self::agent::KitAgent;
#[cfg(feature = "kit-compiler")]
#[cfg_attr(docsrs, doc(cfg(feature = "kit-compiler")))]
pub use self::compiler::{CompileJob, KitCompiler};
pub use self::package::{KitPackage, PackageImage};
