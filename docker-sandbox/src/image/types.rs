//! Payloads of the image operations.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Extra;
use crate::error::{Error, Result};

/// An image, as listed by `GET /docker/images`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageSummary {
    /// Repository, e.g. `docker.io/library/sandbox-kit-assembled`.
    pub repository: String,
    /// Tag, absent for images only referenced by digest.
    #[serde(default)]
    pub tag: Option<String>,
    /// Image identifier (`sha256:…`).
    pub id: String,
    /// Content digest (`sha256:…`).
    #[serde(default)]
    pub digest: Option<String>,
    /// Size in bytes.
    #[serde(default)]
    pub size: Option<u64>,
    /// Creation time (RFC 3339).
    #[serde(default)]
    pub created_at: Option<String>,
    /// Agent of the kit the image belongs to, when known.
    #[serde(default)]
    pub agent: Option<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl ImageSummary {
    /// Reference accepted by the image endpoints: `repository:tag`, or
    /// `repository@digest` for untagged images.
    ///
    /// ```
    /// use docker_sandbox::image::ImageSummary;
    ///
    /// let image: ImageSummary = serde_json::from_value(serde_json::json!({
    ///     "repository": "docker.io/sbx/shell",
    ///     "id": "sha256:f3c1",
    ///     "digest": "sha256:f3c1"
    /// })).unwrap();
    /// assert_eq!(image.reference(), "docker.io/sbx/shell@sha256:f3c1");
    /// ```
    #[must_use]
    pub fn reference(&self) -> String {
        match (&self.tag, &self.digest) {
            (Some(tag), _) if !tag.is_empty() => format!("{}:{tag}", self.repository),
            (_, Some(digest)) => format!("{}@{digest}", self.repository),
            _ => format!("{}@{}", self.repository, self.id),
        }
    }
}

/// Response of `GET /docker/images/inspect`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageInspect {
    /// Image identifier (`sha256:…`).
    pub id: String,
    /// Agent of the kit the image belongs to, when known.
    #[serde(default)]
    pub agent: Option<String>,
    /// Default command.
    #[serde(default)]
    pub cmd: Vec<String>,
    /// Entrypoint.
    #[serde(default)]
    pub entrypoint: Vec<String>,
    /// Default user.
    #[serde(default)]
    pub user: Option<String>,
    /// Creation time (RFC 3339).
    #[serde(default)]
    pub created_at: Option<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Response of `GET /docker/images/local-metadata`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalImageMetadata {
    /// Whether the image is a locally saved snapshot.
    #[serde(default)]
    pub local_snapshot: bool,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// One line of the progress stream of an image pull
/// (`POST /docker/images/create`), in the format of the Docker engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullProgress {
    /// Layer the line refers to, when it refers to one.
    #[serde(default)]
    pub id: Option<String>,
    /// What happens, e.g. `Downloading` or `Download complete`.
    #[serde(default)]
    pub status: Option<String>,
    /// Byte counts of the layer.
    #[serde(default, rename = "progressDetail")]
    pub progress_detail: Option<ProgressDetail>,
    /// Failure of the pull. Always the last line of a failed stream.
    #[serde(default)]
    pub error: Option<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl PullProgress {
    /// Parses one NDJSON line. Returns `None` for blank lines.
    pub(crate) fn parse_line(line: &str) -> Result<Option<Self>> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(None);
        }
        let mut value: Value = serde_json::from_str(line).map_err(|source| Error::Decode {
            operation: "image pull",
            source,
        })?;
        // The engine reports failures as `{"error": …}` or under `errorDetail`.
        if value.get("error").is_none()
            && let Some(message) = value.pointer("/errorDetail/message").cloned()
        {
            value["error"] = message;
        }
        serde_json::from_value(value)
            .map(Some)
            .map_err(|source| Error::Decode {
                operation: "image pull",
                source,
            })
    }
}

/// Byte counts of a layer being pulled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressDetail {
    /// Bytes transferred so far.
    #[serde(default)]
    pub current: Option<u64>,
    /// Size of the layer.
    #[serde(default)]
    pub total: Option<u64>,
}
