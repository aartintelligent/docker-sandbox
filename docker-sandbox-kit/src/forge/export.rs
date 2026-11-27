//! The small files of an exported image, read as the daemon streams them.
//!
//! The descriptor of a published kit is in the image index its export holds
//! (`GET /docker/images/export`, an OCI image layout), and its entrypoint in
//! the image configuration: files of a few kilobytes, next to layers of
//! hundreds of megabytes. The export is read as a stream, and only the files
//! no larger than [`SMALL_FILE`] are kept; the layers are skipped as they go
//! by, never held in memory.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use docker_sandbox::Client;

/// Largest file kept from an export: the index, manifests and configuration
/// are a few kilobytes, layers are skipped.
pub(super) const SMALL_FILE: u64 = 1 << 20;

/// Largest PAX records or GNU long name read for the path of the next entry;
/// larger ones are skipped, so that a name is never held beyond this.
const NAME_LIMIT: u64 = 64 << 10;

/// Operation named by the errors of this module, as by those of the export.
const OPERATION: &str = "image export";

/// Size of a tar block: headers and the padding of file contents.
const BLOCK: usize = 512;

/// The files of the export of `reference` no larger than [`SMALL_FILE`], by
/// path.
///
/// # Errors
///
/// Fails with the errors of the export request, and with [`Error::Client`] ([`docker_sandbox::Error::Archive`])
/// when the stream is not a tar archive or ends in the middle of an entry.
pub(super) async fn small_files(
    client: &Client,
    reference: &str,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut archive = client.images().export_stream(reference).await?;
    let mut scan = TarScan::new(SMALL_FILE);
    while let Some(chunk) = archive.next().await {
        scan.feed(&chunk?)?;
    }
    scan.finish()
}

/// What the bytes being read belong to.
#[derive(Debug)]
enum State {
    /// A header block, being gathered.
    Header,
    /// The contents of an entry, then their padding.
    Contents {
        /// What the contents are.
        kind: Contents,
        /// Bytes of contents still to read.
        remaining: u64,
        /// Bytes of padding after them.
        padding: u64,
    },
    /// The end of the archive: the rest is ignored.
    Ended,
}

/// What the contents of an entry are.
#[derive(Debug)]
enum Contents {
    /// A file kept under its path.
    Kept(String),
    /// PAX records whose `path` names the next entry.
    Pax,
    /// A GNU long name: the path of the next entry.
    LongName,
    /// Anything else: skipped.
    Skipped,
}

/// A tar archive read block by block, keeping the regular files no larger
/// than a limit.
#[derive(Debug)]
struct TarScan {
    /// Largest file kept.
    limit: u64,
    /// Where the reading is.
    state: State,
    /// Bytes of the current header or kept contents, gathered across chunks.
    buffer: Vec<u8>,
    /// Path the next entry takes from a PAX record or a GNU long name.
    next_path: Option<String>,
    /// The kept files, by path.
    files: BTreeMap<String, Vec<u8>>,
}

impl TarScan {
    /// A scan keeping the regular files no larger than `limit`.
    fn new(limit: u64) -> Self {
        Self {
            limit,
            state: State::Header,
            buffer: Vec::new(),
            next_path: None,
            files: BTreeMap::new(),
        }
    }

    /// Reads the next bytes of the archive.
    fn feed(&mut self, mut chunk: &[u8]) -> Result<()> {
        while !chunk.is_empty() {
            match &mut self.state {
                State::Ended => return Ok(()),
                State::Header => {
                    let wanted = BLOCK - self.buffer.len();
                    let taken = wanted.min(chunk.len());
                    self.buffer.extend_from_slice(&chunk[..taken]);
                    chunk = &chunk[taken..];
                    if self.buffer.len() == BLOCK {
                        let header = std::mem::take(&mut self.buffer);
                        self.state = self.header(&header)?;
                    }
                }
                State::Contents {
                    kind,
                    remaining,
                    padding,
                } => {
                    let wanted = usize::try_from(*remaining).unwrap_or(usize::MAX);
                    let taken = wanted.min(chunk.len());
                    if !matches!(kind, Contents::Skipped) {
                        self.buffer.extend_from_slice(&chunk[..taken]);
                    }
                    chunk = &chunk[taken..];
                    *remaining -= taken as u64;
                    if *remaining == 0 {
                        let skip = usize::try_from(*padding).unwrap_or(usize::MAX);
                        let skipped = skip.min(chunk.len());
                        chunk = &chunk[skipped..];
                        *padding -= skipped as u64;
                        if *padding == 0 {
                            let kind = std::mem::replace(kind, Contents::Skipped);
                            self.end_of_entry(kind);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The state after the header block `header`.
    fn header(&mut self, header: &[u8]) -> Result<State> {
        if header.iter().all(|&b| b == 0) {
            return Ok(State::Ended);
        }
        let size = octal(&header[124..136])?;
        let path = self.next_path.take().unwrap_or_else(|| {
            let name = text(&header[0..100]);
            // Only POSIX headers (`ustar\0`) have a prefix field: GNU ones
            // (`ustar `) keep their dates there.
            let prefix = if &header[257..263] == b"ustar\0" {
                text(&header[345..500])
            } else {
                String::new()
            };
            if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            }
        });
        let kind = match header[156] {
            b'x' if size <= NAME_LIMIT => Contents::Pax,
            b'L' if size <= NAME_LIMIT => Contents::LongName,
            b'0' | 0 if size <= self.limit => Contents::Kept(path),
            _ => Contents::Skipped,
        };
        if size == 0 {
            // An empty entry ends with its header.
            self.end_of_entry(kind);
            return Ok(State::Header);
        }
        let padding = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
        Ok(State::Contents {
            kind,
            remaining: size,
            padding,
        })
    }

    /// Files the contents just read, of an entry of `kind`.
    fn end_of_entry(&mut self, kind: Contents) {
        let contents = std::mem::take(&mut self.buffer);
        match kind {
            Contents::Kept(path) => {
                self.files.insert(path, contents);
            }
            Contents::Pax => {
                self.next_path = pax_path(&contents);
            }
            Contents::LongName => {
                self.next_path = Some(text(&contents));
            }
            Contents::Skipped => {}
        }
        self.state = State::Header;
    }

    /// The kept files, once the archive is read.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Client`] ([`docker_sandbox::Error::Archive`]) when the archive ended in the middle of an
    /// entry.
    fn finish(self) -> Result<BTreeMap<String, Vec<u8>>> {
        match self.state {
            State::Header if self.buffer.is_empty() => Ok(self.files),
            State::Ended => Ok(self.files),
            _ => Err(archive_error("the export ended in the middle of an entry")),
        }
    }
}

/// The text of a NUL-padded header field.
fn text(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// The number of an octal header field, padded with spaces or NULs.
fn octal(field: &[u8]) -> Result<u64> {
    let digits = text(field);
    let digits = digits.trim_matches(|c: char| c == ' ' || c == '\0');
    if digits.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(digits, 8)
        .map_err(|_| archive_error(&format!("invalid size field {digits:?} in the export")))
}

/// The `path` of PAX records (`<length> path=<value>\n` …), if they hold one.
fn pax_path(records: &[u8]) -> Option<String> {
    String::from_utf8_lossy(records)
        .lines()
        .filter_map(|record| record.split_once(' ').map(|(_, field)| field))
        .find_map(|field| field.strip_prefix("path=").map(str::to_owned))
}

/// A [`docker_sandbox::Error::Archive`] of this module, as an [`Error::Client`].
fn archive_error(message: &str) -> Error {
    Error::Client(docker_sandbox::Error::Archive {
        operation: OPERATION,
        message: message.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tar archive of `(path, contents)` entries, with GNU headers, so that
    /// long paths take a GNU long name entry.
    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            builder.append_data(&mut header, path, *contents).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// The kept files of `bytes` fed in chunks of `size` bytes.
    fn scan(bytes: &[u8], limit: u64, size: usize) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut scan = TarScan::new(limit);
        for chunk in bytes.chunks(size) {
            scan.feed(chunk)?;
        }
        scan.finish()
    }

    #[test]
    fn small_files_are_kept_whatever_the_chunks() {
        let long = format!("blobs/sha256/{}", "a".repeat(120));
        let big = vec![7_u8; 3000];
        let bytes = archive(&[
            ("index.json", b"{}"),
            ("layer", &big),
            (&long, b"long"),
            ("empty", b""),
        ]);
        for size in [1, 7, 512, 1000, bytes.len()] {
            let files = scan(&bytes, 100, size).unwrap();
            assert_eq!(files["index.json"], b"{}", "chunks of {size}");
            assert_eq!(files[&long], b"long", "chunks of {size}");
            assert!(files["empty"].is_empty());
            assert!(!files.contains_key("layer"), "larger than the limit");
        }
    }

    #[test]
    fn pax_paths_and_ustar_prefixes_name_their_entries() {
        let mut builder = tar::Builder::new(Vec::new());
        let path = format!("{}/manifest.json", "d".repeat(150));
        let mut header = tar::Header::new_ustar();
        header.set_size(2);
        header.set_mode(0o644);
        builder.append_data(&mut header, &path, &b"ok"[..]).unwrap();
        builder
            .append_pax_extensions([("path", b"pax/name.json".as_slice())])
            .unwrap();
        let mut header = tar::Header::new_ustar();
        header.set_size(3);
        header.set_mode(0o644);
        builder
            .append_data(&mut header, "short", &b"pax"[..])
            .unwrap();
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        builder.append_data(&mut header, "dir/", &[][..]).unwrap();
        let files = scan(&builder.into_inner().unwrap(), 100, 64).unwrap();
        assert_eq!(files[&path], b"ok");
        assert_eq!(files["pax/name.json"], b"pax");
        assert_eq!(files.len(), 2, "directories are not kept");
    }

    #[test]
    fn old_headers_name_regular_files_with_a_nul_type() {
        let mut header = tar::Header::new_old();
        header.set_size(2);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::new(0));
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_data(&mut header, "index.json", &b"{}"[..])
            .unwrap();
        let files = scan(&builder.into_inner().unwrap(), 100, 512).unwrap();
        assert_eq!(files["index.json"], b"{}");
    }

    #[test]
    fn gnu_headers_keep_their_dates_out_of_the_path() {
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o644);
        header.as_gnu_mut().unwrap().set_atime(1_700_000_000);
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_data(&mut header, "index.json", &b"{}"[..])
            .unwrap();
        let files = scan(&builder.into_inner().unwrap(), 100, 512).unwrap();
        assert_eq!(files.keys().collect::<Vec<_>>(), ["index.json"]);
    }

    #[test]
    fn archives_cut_short_or_malformed_are_errors() {
        let bytes = archive(&[("index.json", b"{\"a\":1}")]);
        assert!(scan(&bytes[..600], 100, 64).is_err(), "cut in the contents");
        assert!(scan(&bytes[..100], 100, 64).is_err(), "cut in a header");
        assert!(scan(&bytes[..1024], 100, 64).is_ok(), "cut between entries");
        let mut bad = bytes;
        bad[124..136].copy_from_slice(b"zzzzzzzzzzz\0");
        let error = scan(&bad, 100, 512).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("image export: invalid size field"),
            "{error}"
        );
    }

    #[test]
    fn header_fields_are_read_as_tar_writes_them() {
        assert_eq!(octal(b"00000000012\0").unwrap(), 10);
        assert_eq!(octal(b"      12 \0\0\0").unwrap(), 10);
        assert_eq!(octal(b"\0\0\0").unwrap(), 0);
        assert_eq!(text(b"name\0\0junk"), "name");
        assert_eq!(
            pax_path(b"30 mtime=1\n25 path=a/b.json\n"),
            Some("a/b.json".to_owned())
        );
        assert_eq!(pax_path(b"30 mtime=1\n"), None);
    }
}
