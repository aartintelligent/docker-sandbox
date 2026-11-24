//! File transfers between the host and a sandbox.
//!
//! The daemon exchanges tar archives (`PUT` and `GET /sandbox/{name}/files`):
//! it extracts an upload into an existing directory, as root, and answers a
//! download with an archive whose entries are named after the last component
//! of the requested path, as `docker cp` does. The helpers build and read
//! those archives in memory, so they suit configuration files and source
//! trees rather than very large data sets; host file access runs on the
//! blocking thread pool.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use super::Sandbox;
use crate::error::{Error, Result};
use crate::transport::{Request, TAR};

/// Mode of the files written by [`Sandbox::write_file`].
const DEFAULT_FILE_MODE: u32 = 0o644;

impl Sandbox {
    /// Extracts a tar archive into the existing directory `dir` of the sandbox
    /// (`PUT /sandbox/{name}/files?path=…`). Files are created as root.
    ///
    /// # Errors
    ///
    /// Fails with a conflict [`Error::Api`] if the sandbox is stopped. A
    /// directory that does not exist, or an archive that cannot be
    /// extracted, is answered with a server error (500, `tar extract
    /// failed`).
    pub async fn upload_archive(&self, dir: &str, archive: impl Into<Bytes>) -> Result<()> {
        let request = Request::put("file upload", self.path("files"))
            .query("path", dir)
            .body(archive.into(), TAR)
            .long_running();
        self.transport().unit(request).await
    }

    /// Reads `path` of the sandbox as a tar archive
    /// (`GET /sandbox/{name}/files?path=…`). The entries are named after the
    /// last component of `path`.
    ///
    /// # Errors
    ///
    /// Fails with a conflict [`Error::Api`] if the sandbox is stopped, and
    /// with a not-found one if the path does not exist.
    pub async fn download_archive(&self, path: &str) -> Result<Bytes> {
        let request = Request::get("file download", self.path("files"))
            .query("path", path)
            .accept(TAR)
            .long_running();
        self.transport().bytes(request).await
    }

    /// Writes `contents` to the file at the absolute `path` of the sandbox,
    /// with mode `0644`. The parent directory must exist; an existing file is
    /// replaced.
    ///
    /// ```no_run
    /// # async fn run(sandbox: docker_sandbox::sandbox::Sandbox) -> docker_sandbox::Result<()> {
    /// sandbox.write_file("/home/agent/.config/app.toml", "debug = true\n").await?;
    /// let contents = sandbox.read_file("/home/agent/.config/app.toml").await?;
    /// assert_eq!(&contents[..], b"debug = true\n");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Fails with [`Error::InvalidRequest`] if `path` is not an absolute file
    /// path, and as [`Sandbox::upload_archive`] otherwise: a conflict if the
    /// sandbox is stopped, a server error if the directory does not exist.
    pub async fn write_file(&self, path: &str, contents: impl Into<Bytes>) -> Result<()> {
        self.write_file_with_mode(path, contents, DEFAULT_FILE_MODE)
            .await
    }

    /// Writes `contents` to the file at the absolute `path` of the sandbox,
    /// with the Unix permissions `mode` (for instance `0o755` for a script).
    ///
    /// # Errors
    ///
    /// See [`Sandbox::write_file`].
    pub async fn write_file_with_mode(
        &self,
        path: &str,
        contents: impl Into<Bytes>,
        mode: u32,
    ) -> Result<()> {
        let (dir, name) = split_file_path(path)?;
        let archive = single_file_archive(name, &contents.into(), mode)?;
        self.upload_archive(dir, archive).await
    }

    /// Reads the regular file at `path` of the sandbox.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Archive`] if `path` is not a regular file, and as
    /// [`Sandbox::download_archive`] otherwise.
    pub async fn read_file(&self, path: &str) -> Result<Bytes> {
        let archive = self.download_archive(path).await?;
        root_file(&archive)?.ok_or_else(|| Error::Archive {
            operation: "file download",
            message: format!("{path} is not a regular file"),
        })
    }

    /// Copies the host directory `host_dir` into the existing directory `dir`
    /// of the sandbox: `upload_dir("./src", "/work")` creates `/work/src`.
    /// Symbolic links are copied as links.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::File`] if `host_dir` cannot be read, and with the
    /// daemon error if the sandbox is not running or `dir` does not exist.
    pub async fn upload_dir(&self, host_dir: impl AsRef<Path>, dir: &str) -> Result<()> {
        let host_dir = host_dir.as_ref().to_owned();
        let archive = blocking(move || directory_archive(&host_dir)).await?;
        self.upload_archive(dir, archive).await
    }

    /// Copies the directory or file at `path` of the sandbox into the host
    /// directory `host_dir`, created when missing:
    /// `download_dir("/work/out", "./results")` creates `./results/out`.
    ///
    /// Entries that would land outside `host_dir` are skipped.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::File`] if `host_dir` cannot be written, and with the
    /// daemon error if the sandbox is not running or the path does not exist.
    pub async fn download_dir(&self, path: &str, host_dir: impl AsRef<Path>) -> Result<()> {
        let archive = self.download_archive(path).await?;
        let host_dir = host_dir.as_ref().to_owned();
        blocking(move || unpack(&archive, &host_dir)).await
    }
}

/// Splits an absolute file path into its directory and file name.
fn split_file_path(path: &str) -> Result<(&str, &str)> {
    let invalid = || Error::InvalidRequest(format!("{path:?} is not an absolute file path"));
    if !path.starts_with('/') || path.ends_with('/') {
        return Err(invalid());
    }
    let (dir, name) = path.rsplit_once('/').ok_or_else(invalid)?;
    if name.is_empty() || name == "." || name == ".." {
        return Err(invalid());
    }
    Ok((if dir.is_empty() { "/" } else { dir }, name))
}

/// A tar archive holding one regular file.
fn single_file_archive(name: &str, contents: &[u8], mode: u32) -> Result<Bytes> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(contents.len() as u64);
    header.set_mode(mode);
    header.set_mtime(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    );
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_data(&mut header, name, contents)
        .and_then(|()| builder.into_inner())
        .map(Bytes::from)
        .map_err(|e| Error::Archive {
            operation: "file upload",
            message: format!("cannot build the archive of {name}: {e}"),
        })
}

/// Contents of the root entry of `archive` when it is a regular file.
fn root_file(archive: &[u8]) -> Result<Option<Bytes>> {
    let unreadable = |e: std::io::Error| Error::Archive {
        operation: "file download",
        message: format!("unreadable archive: {e}"),
    };
    let mut archive = tar::Archive::new(archive);
    let Some(entry) = archive.entries().map_err(unreadable)?.next() else {
        return Ok(None);
    };
    let mut entry = entry.map_err(unreadable)?;
    if !entry.header().entry_type().is_file() {
        return Ok(None);
    }
    let mut contents = Vec::new();
    entry.read_to_end(&mut contents).map_err(unreadable)?;
    Ok(Some(Bytes::from(contents)))
}

/// A tar archive of `host_dir`, its entries named after its last component.
fn directory_archive(host_dir: &Path) -> Result<Bytes> {
    let file_error = |source| Error::File {
        path: host_dir.to_owned(),
        source,
    };
    let canonical = host_dir.canonicalize().map_err(file_error)?;
    let name = canonical.file_name().ok_or_else(|| {
        Error::InvalidRequest(format!("{} has no directory name", host_dir.display()))
    })?;
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    builder
        .append_dir_all(name, &canonical)
        .and_then(|()| builder.into_inner())
        .map(Bytes::from)
        .map_err(file_error)
}

/// Extracts `archive` into `host_dir`, created when missing.
fn unpack(archive: &[u8], host_dir: &Path) -> Result<()> {
    let file_error = |source| Error::File {
        path: host_dir.to_owned(),
        source,
    };
    std::fs::create_dir_all(host_dir).map_err(file_error)?;
    tar::Archive::new(archive)
        .unpack(host_dir)
        .map_err(file_error)
}

/// Runs blocking host file access on the blocking thread pool.
async fn blocking<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Error::File {
            path: PathBuf::new(),
            source: std::io::Error::other(e),
        })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_paths_must_be_absolute_and_name_a_file() {
        assert_eq!(
            split_file_path("/etc/app.toml").unwrap(),
            ("/etc", "app.toml")
        );
        assert_eq!(split_file_path("/app.toml").unwrap(), ("/", "app.toml"));
        for invalid in ["app.toml", "/etc/", "/etc/..", "/", ""] {
            assert!(
                matches!(split_file_path(invalid), Err(Error::InvalidRequest(_))),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn single_files_round_trip_through_an_archive() {
        let archive = single_file_archive("run.sh", b"echo ok\n", 0o755).unwrap();
        let mut entries = tar::Archive::new(&archive[..]);
        let entry = entries.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap(), Path::new("run.sh"));
        assert_eq!(entry.header().mode().unwrap(), 0o755);
        assert_eq!(
            root_file(&archive).unwrap().as_deref(),
            Some(&b"echo ok\n"[..])
        );
    }

    #[test]
    fn directories_are_archived_under_their_name_and_unpacked() {
        let source = tempfile::tempdir().unwrap();
        let tree = source.path().join("src");
        std::fs::create_dir_all(tree.join("nested")).unwrap();
        std::fs::write(tree.join("nested/a.txt"), "a").unwrap();

        let archive = directory_archive(&tree).unwrap();
        let target = tempfile::tempdir().unwrap();
        unpack(&archive, &target.path().join("out")).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.path().join("out/src/nested/a.txt")).unwrap(),
            "a"
        );
        assert!(
            root_file(&archive).unwrap().is_none(),
            "a directory is not a file"
        );
    }

    #[test]
    fn names_a_tar_header_cannot_hold_are_archive_errors() {
        assert!(matches!(
            single_file_archive("a\0b", b"", 0o644),
            Err(Error::Archive {
                operation: "file upload",
                ..
            })
        ));
    }

    #[test]
    fn empty_and_corrupt_archives_hold_no_file() {
        assert!(root_file(&[]).unwrap().is_none(), "no entry");
        assert!(matches!(
            root_file(&[0xff; 512]),
            Err(Error::Archive {
                operation: "file download",
                ..
            })
        ));
    }

    #[test]
    fn the_root_directory_has_no_name_to_archive_under() {
        assert!(matches!(
            directory_archive(Path::new("/")),
            Err(Error::InvalidRequest(_))
        ));
    }

    #[test]
    fn unpacking_below_a_file_is_a_file_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let archive = single_file_archive("a.txt", b"a", 0o644).unwrap();
        assert!(matches!(
            unpack(&archive, &file.join("out")),
            Err(Error::File { ref path, .. }) if *path == file.join("out")
        ));
    }

    #[tokio::test]
    async fn a_panic_in_blocking_work_is_reported_as_an_error() {
        let result: Result<()> = blocking(|| panic!("tar panicked")).await;
        assert!(matches!(result, Err(Error::File { .. })));
    }
}
