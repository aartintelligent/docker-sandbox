//! Custom kits: kit packages, replayed through the socket alone.
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
//! Packages are written by the `docker-sandbox-kit` crate: forged through the
//! socket alone (`KitForge`), the base kit rebuilt from its published image and
//! mixins described by a kit descriptor, or compiled once with the CLI
//! (`KitCompiler`). `audit kit-compile` writes them too. A compiled package is
//! tied to the `sbx` version that compiled it, a forged one to the API of the
//! daemon it was forged for.

mod agent;
mod package;

pub use self::agent::KitAgent;
pub use self::package::{KitPackage, PackageImage};
