//! Images held by the daemon: kit images, assembled sandbox images and saved
//! sandboxes.
//!
//! Obtained with [`Client::images`](crate::Client::images). Image references
//! are passed as the daemon prints them, e.g. `docker.io/sbx/shell@sha256:…`
//! or `docker.io/library/snap:v1`.

mod export;
mod types;

use std::path::Path;

use bytes::Bytes;

pub use self::export::ExportStream;
pub use self::types::{
    ImageInspect, ImageSummary, LocalImageMetadata, ProgressDetail, PullProgress,
};
use crate::client::Client;
use crate::error::{Error, Result};
use crate::transport::{NdjsonStream, OCTET_STREAM, Request, TAR};

/// Operation named by the export requests and their errors.
const EXPORT: &str = "image export";

/// Progress stream of an image pull, returned by [`Images::pull_stream`].
pub type PullStream = NdjsonStream<PullProgress>;

/// Operations on the images held by the daemon. Obtained with
/// [`Client::images`].
#[derive(Debug, Clone)]
pub struct Images {
    /// Client the requests go through.
    client: Client,
}

impl Images {
    /// Image operations through `client`.
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Every image, including intermediate tags (`GET /docker/images?all=true`).
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn list(&self) -> Result<Vec<ImageSummary>> {
        let request = Request::get("image list", "/docker/images").query("all", "true");
        self.client.transport().json(request).await
    }

    /// Details of an image (`GET /docker/images/inspect?name=…`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`] if the image is not present.
    pub async fn inspect(&self, reference: &str) -> Result<ImageInspect> {
        let request =
            Request::get("image inspection", "/docker/images/inspect").query("name", reference);
        self.client.transport().json(request).await
    }

    /// Whether the daemon holds the image `reference`.
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error other than
    /// not found.
    pub async fn exists(&self, reference: &str) -> Result<bool> {
        match self.inspect(reference).await {
            Ok(_) => Ok(true),
            Err(error) if error.is_not_found() => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Local metadata of an image (`GET /docker/images/local-metadata?name=…`).
    ///
    /// An image the daemon does not hold is answered with the defaults
    /// (`local_snapshot: false`), not an error: use [`Images::exists`] to tell.
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn local_metadata(&self, reference: &str) -> Result<LocalImageMetadata> {
        let request = Request::get("image local metadata", "/docker/images/local-metadata")
            .query("name", reference);
        self.client.transport().json(request).await
    }

    /// Pulls an image and waits for the outcome
    /// (`POST /docker/images/create?fromImage=…`).
    ///
    /// A cached copy is used when the registry refuses the pull, as the CLI
    /// does.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Api`] when the pull is refused, for instance
    /// `403 registry_access_denied` for an image the registry does not serve,
    /// and with [`Error::Failed`] when a failure is reported in the progress
    /// stream instead.
    pub async fn pull(&self, reference: &str) -> Result<()> {
        let mut stream = self.pull_stream(reference).await?;
        while let Some(progress) = stream.next().await {
            if let Some(message) = progress?.error {
                return Err(Error::Failed {
                    operation: "image pull",
                    message,
                });
            }
        }
        Ok(())
    }

    /// Starts an image pull and returns its progress as it happens.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Api`] if the daemon rejects the request before
    /// streaming.
    pub async fn pull_stream(&self, reference: &str) -> Result<PullStream> {
        let request = Request::post("image pull", "/docker/images/create")
            .query("allowCachedOnForbidden", "true")
            .query("fromImage", reference)
            .long_running();
        self.client
            .transport()
            .ndjson(request, PullProgress::parse_line)
            .await
    }

    /// Loads an image archive (OCI layout or Docker save format) into the
    /// daemon (`POST /docker/images/load`).
    ///
    /// This is how local kit sources are made available before creating a
    /// sandbox from a compiled kit package.
    ///
    /// # Errors
    ///
    /// Fails with a server error [`Error::Api`] (500, `failed to complete
    /// image load`) for an archive the daemon cannot load.
    pub async fn load(&self, archive: impl Into<Bytes>) -> Result<()> {
        let request = Request::post("image load", "/docker/images/load")
            .body(archive.into(), OCTET_STREAM)
            .long_running();
        self.client.transport().unit(request).await
    }

    /// Loads the image archive stored at `path` on the host.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::File`] if the file cannot be read, and as
    /// [`Images::load`] otherwise.
    pub async fn load_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref().to_owned();
        let archive = tokio::fs::read(&path)
            .await
            .map_err(|source| Error::File { path, source })?;
        self.load(archive).await
    }

    /// Exports an image as a tar archive (`GET /docker/images/export?name=…`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`] if the image is not present.
    pub async fn export(&self, reference: &str) -> Result<Bytes> {
        self.client
            .transport()
            .bytes(export_request(reference))
            .await
    }

    /// Exports an image as a tar archive read as the daemon sends it
    /// (`GET /docker/images/export?name=…`): unlike [`Images::export`], the
    /// archive is never held whole in memory.
    ///
    /// ```no_run
    /// # async fn run(client: docker_sandbox::Client) -> docker_sandbox::Result<()> {
    /// let mut archive = client.images().export_stream("docker.io/sbx/shell:latest").await?;
    /// let mut size = 0;
    /// while let Some(chunk) = archive.next().await {
    ///     size += chunk?.len();
    /// }
    /// println!("{size} bytes");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`] if the image is not present. A
    /// chunk fails with [`Error::Http`] when the connection breaks in the
    /// middle of the archive.
    pub async fn export_stream(&self, reference: &str) -> Result<ExportStream> {
        let body = self
            .client
            .transport()
            .stream(export_request(reference))
            .await?;
        Ok(ExportStream::new(EXPORT, body))
    }

    /// Removes an image reference (`DELETE /docker/images/remove?name=…`).
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`] if the reference does not exist.
    pub async fn remove(&self, reference: &str) -> Result<()> {
        let request =
            Request::delete("image removal", "/docker/images/remove").query("name", reference);
        self.client.transport().unit(request).await
    }
}

/// The request exporting the image `reference` as a tar archive.
fn export_request(reference: &str) -> Request {
    Request::get(EXPORT, "/docker/images/export")
        .query("name", reference)
        .accept(TAR)
        .long_running()
}
