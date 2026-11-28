//! References read from their name as the `sbx` CLI reads them: a kit, an
//! agent, an image, a directory or a Git repository ([`KitRef`]).
//!
//! `sbx create <kit>` and `sbx create --kit <kit>` read a kit from one text,
//! by rules the audit of this repository measured on `sbx` v0.54.1, which the
//! base kit and the mixins do not share: a name without a slash is an agent
//! as a base kit, and a mixin only when a directory of that name exists. A
//! program that takes kits as the CLI does reads them with [`KitRef::base`]
//! and [`KitRef::mixin`] rather than guess.

use std::path::{Path, PathBuf};

use crate::Agent;
use crate::error::{Error, Result};

/// The prefix of a Git repository, over HTTPS or SSH.
const GIT: &str = "git+";

/// The prefix of an explicit OCI reference.
const OCI: &str = "oci://";

/// A kit, as the `sbx` CLI reads it from its name.
///
/// ```
/// use docker_sandbox_kit::{Agent, KitRef};
///
/// assert_eq!(KitRef::base("claude")?, KitRef::Agent(Agent::Claude));
/// assert_eq!(KitRef::base("ghcr.io/acme/kit:1")?, KitRef::Image("ghcr.io/acme/kit:1".into()));
/// assert!(KitRef::base("acme").is_err(), "not an agent sbx knows");
/// # Ok::<(), docker_sandbox_kit::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum KitRef {
    /// An agent `sbx` knows, `claude`: its published kit, or the kit its
    /// shortcut on the daemon names.
    Agent(Agent),
    /// An image of a registry, `ghcr.io/acme/kit:1`, as written but for an
    /// `oci://` prefix.
    Image(String),
    /// A directory or a ZIP file of the host, `~` expanded.
    Local(PathBuf),
    /// A Git repository, as written, e.g.
    /// `git+https://host/org/repo.git#ref=v1.0&dir=path`.
    Git(String),
}

impl KitRef {
    /// The kit `sbx create <text>` takes as base kit: a Git repository
    /// (`git+https://…`, `git+ssh://…`), an image (`oci://…`, or a name with
    /// a slash, even when a directory of that name exists), a path written
    /// as one (`./…`, `../…`, `/…`, `~/…`), and otherwise an agent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidKit`] for a name without a slash that is not
    /// an agent `sbx` knows, as the CLI refuses it, and for an empty text.
    pub fn base(text: &str) -> Result<Self> {
        if let Some(kit) = Self::explicit(text, home().as_deref())? {
            return Ok(kit);
        }
        if text.contains('/') {
            return Ok(Self::Image(text.to_owned()));
        }
        text.parse::<Agent>()
            .map(Self::Agent)
            .map_err(|_| Error::InvalidKit {
                kit: text.to_owned(),
                message: "unknown agent: a base kit named without a slash is an agent sbx knows"
                    .to_owned(),
            })
    }

    /// The kit `sbx create --kit <text>` takes as mixin: a Git repository, an
    /// image written `oci://…`, a path written as one, a path relative to the
    /// current directory that exists, `tools` or `kits/tools` included, and
    /// otherwise an image when the name holds a slash.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidKit`] for a name without a slash that names no
    /// directory, which the CLI takes neither for a path nor for an image,
    /// and for an empty text.
    pub fn mixin(text: &str) -> Result<Self> {
        let current = std::env::current_dir().unwrap_or_default();
        Self::mixin_in(text, &current, home().as_deref())
    }

    /// [`KitRef::mixin`], relative to the directory `current`, with the home
    /// directory `home`.
    fn mixin_in(text: &str, current: &Path, home: Option<&Path>) -> Result<Self> {
        if let Some(kit) = Self::explicit(text, home)? {
            return Ok(kit);
        }
        if current.join(text).exists() {
            return Ok(Self::Local(PathBuf::from(text)));
        }
        if text.contains('/') {
            return Ok(Self::Image(text.to_owned()));
        }
        Err(Error::InvalidKit {
            kit: text.to_owned(),
            message: "neither a path that exists nor an image: write a directory ./kit, a ZIP \
                      file ./kit.zip, an image registry.example/org/kit:tag or oci://…, or a \
                      Git repository git+https://host/org/repo.git"
                .to_owned(),
        })
    }

    /// The kit `text` names by its form alone, the same for a base kit and a
    /// mixin: a Git repository, an `oci://` image, or a path written as one,
    /// `~` expanded with `home`.
    fn explicit(text: &str, home: Option<&Path>) -> Result<Option<Self>> {
        if text.is_empty() {
            return Err(Error::InvalidKit {
                kit: String::new(),
                message: "a kit is named by a text that is not empty".to_owned(),
            });
        }
        if text.starts_with(GIT) {
            return Ok(Some(Self::Git(text.to_owned())));
        }
        if let Some(image) = text.strip_prefix(OCI) {
            return Ok(Some(Self::Image(image.to_owned())));
        }
        if let Some(rest) = text.strip_prefix("~/") {
            let home = home.ok_or_else(|| Error::InvalidKit {
                kit: text.to_owned(),
                message: "~ names the home directory, and HOME is not set".to_owned(),
            })?;
            return Ok(Some(Self::Local(home.join(rest))));
        }
        let path = ["./", "../", "/"]
            .iter()
            .any(|prefix| text.starts_with(prefix));
        Ok(path.then(|| Self::Local(PathBuf::from(text))))
    }
}

/// The home directory of the user, from `HOME`.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_kits_are_read_as_the_cli_reads_them() {
        let cases = [
            ("claude", KitRef::Agent(Agent::Claude)),
            ("docker-agent", KitRef::Agent(Agent::DockerAgent)),
            (
                "ghcr.io/acme/kit:1",
                KitRef::Image("ghcr.io/acme/kit:1".into()),
            ),
            // A slash makes an image, even when a directory of that name exists.
            ("kits/tools", KitRef::Image("kits/tools".into())),
            (
                "oci://ghcr.io/acme/kit",
                KitRef::Image("ghcr.io/acme/kit".into()),
            ),
            ("./kits/tools", KitRef::Local("./kits/tools".into())),
            ("../kit.zip", KitRef::Local("../kit.zip".into())),
            ("/opt/kits/tools", KitRef::Local("/opt/kits/tools".into())),
            (
                "git+https://host/org/repo.git#ref=v1.0&dir=path",
                KitRef::Git("git+https://host/org/repo.git#ref=v1.0&dir=path".into()),
            ),
            (
                "git+ssh://git@host/org/repo.git",
                KitRef::Git("git+ssh://git@host/org/repo.git".into()),
            ),
        ];
        for (text, kit) in cases {
            assert_eq!(KitRef::base(text).unwrap(), kit, "{text}");
        }
        for refused in ["acme", "foo:1"] {
            let error = KitRef::base(refused).unwrap_err().to_string();
            assert!(error.contains("unknown agent"), "{refused}: {error}");
        }
        assert!(KitRef::base("").is_err());
    }

    #[test]
    fn mixins_are_read_as_the_cli_reads_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("kits/tools")).unwrap();
        std::fs::create_dir(dir.path().join("tools")).unwrap();
        let home = Path::new("/srv/user");
        let mixin = |text: &str| KitRef::mixin_in(text, dir.path(), Some(home));
        let cases = [
            // Relative paths that exist, with or without ./, are directories.
            ("tools", KitRef::Local("tools".into())),
            ("kits/tools", KitRef::Local("kits/tools".into())),
            ("./kits/tools", KitRef::Local("./kits/tools".into())),
            ("~/kits/tools", KitRef::Local("/srv/user/kits/tools".into())),
            // One that does not exist is an image when it holds a slash.
            ("kits/none", KitRef::Image("kits/none".into())),
            ("oci://sbx/shell", KitRef::Image("sbx/shell".into())),
            (
                "git+https://host/org/repo.git",
                KitRef::Git("git+https://host/org/repo.git".into()),
            ),
        ];
        for (text, kit) in cases {
            assert_eq!(mixin(text).unwrap(), kit, "{text}");
        }
        for refused in ["none", "tools:1"] {
            let error = mixin(refused).unwrap_err().to_string();
            assert!(
                error.contains("neither a path that exists nor an image"),
                "{refused}: {error}"
            );
        }
        let error = KitRef::mixin_in("~/kits", dir.path(), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("HOME is not set"), "{error}");
        assert!(KitRef::mixin("").is_err());
    }
}
