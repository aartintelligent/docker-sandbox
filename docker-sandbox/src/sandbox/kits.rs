//! Adding kits to an existing sandbox.
//!
//! The daemon offers two ways, both fed with the `kit_artifacts` the `sbx` CLI
//! assembles, and both driven from a compiled package by
//! [`KitPackage`](crate::kit::KitPackage):
//!
//! - **in place** (`POST /sandbox/{name}/kits`): the install commands of one
//!   kit run in the running container. Fast, but the kit list of the sandbox
//!   is not updated;
//! - **by recreation** (`POST /sandbox/{name}/swap-container`): the container
//!   is committed, then recreated with the complete kit list. Changes made
//!   inside the sandbox are kept and the kit list is updated.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Sandbox;
use crate::Extra;
use crate::error::Result;
use crate::transport::Request;

/// Body of `POST /sandbox/{name}/swap-container`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwapRequest {
    /// Name of the replacement container. The `sbx` CLI uses
    /// `<sandbox>-swap-<random>`.
    pub docker_name_override: String,
    /// Resolved artifacts of the complete kit list, base kit first. Opaque.
    pub kit_artifacts: Vec<Value>,
    /// References of the complete kit list, base kit first.
    pub kit_refs: Vec<String>,
}

/// Response of `POST /sandbox/{name}/swap-container`.
///
/// The daemon commits the current container to an image (so that changes made
/// inside the sandbox are kept), then recreates it with the new kit list. The
/// sandbox keeps its identifier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwapOutcome {
    /// Image the previous container was committed to.
    #[serde(default)]
    pub committed_image_id: Option<String>,
    /// Identifier of the new container.
    #[serde(default)]
    pub container_id: Option<String>,
    /// Name of the new container.
    #[serde(default)]
    pub docker_name: Option<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Sandbox {
    /// Installs one kit into the running sandbox
    /// (`POST /sandbox/{name}/kits`): its install commands run in the current
    /// container, which is not recreated. The kit list of the sandbox (its
    /// `com.docker.sandbox.kits` label) is not updated.
    ///
    /// `artifact` is one entry of the `kit_artifacts` of a creation request;
    /// the source image of a local kit must be loaded first.
    /// [`KitPackage::install_into`](crate::kit::KitPackage::install_into) does
    /// both from a compiled package.
    ///
    /// # Errors
    ///
    /// Fails with an invalid-input [`Error::Api`](crate::Error::Api) for a
    /// malformed artifact, and with the daemon error if the sandbox is not
    /// running or the kit cannot be installed.
    pub async fn add_kit_artifact(&self, artifact: &Value) -> Result<()> {
        let request = Request::post("kit installation", self.path("kits"))
            .json(&json!({ "kit_artifact": artifact }))?
            .long_running();
        self.transport().unit(request).await
    }

    /// Recreates the sandbox container with a new kit list
    /// (`POST /sandbox/{name}/swap-container`). Changes made inside the
    /// sandbox are kept and the sandbox keeps its identifier.
    ///
    /// The daemon asks for no approval: the network rules the new kits declare
    /// apply at once, where `sbx kit add` first asks the user to approve the
    /// permissions they add. A program that lets others choose the kits must
    /// make that decision itself.
    ///
    /// # Errors
    ///
    /// Fails if the request is rejected or the new container cannot be
    /// started, and with a conflict [`Error::Api`](crate::Error::Api) (`409`)
    /// when the new kit list would change the credentials of the sandbox,
    /// which the running proxy cannot take up: create the sandbox again
    /// instead.
    pub async fn swap_container(&self, request: &SwapRequest) -> Result<SwapOutcome> {
        let request = Request::post("container swap", self.path("swap-container"))
            .json(request)?
            .long_running();
        self.transport().json(request).await
    }
}
