//! Personal data scrubbing and checking.
//!
//! Everything the audit stores (captures, event streams, scenario results,
//! generated contracts, test fixtures) goes through a [`Scrubber`] before it is
//! written, and a snapshot is only kept once [`check`] finds nothing left.
//!
//! What counts as personal data:
//!
//! - the home directory, the user name and the host name of the machine the
//!   audit runs on, read from the environment (none on a GitHub Actions
//!   runner, whose names are nobody's);
//! - e-mail addresses;
//! - host session details the `sbx` CLI forwards to the daemon: the values of
//!   the `SBX_HOST_*` variables (display, D-Bus address, X authority file) and
//!   D-Bus GUIDs;
//! - any value given with `--private` or in `DOCKER_SANDBOX_AUDIT_PRIVATE`
//!   (comma-separated), such as a Docker account name.
//!
//! Replacements are stable (`/home/user`, `user`, `host`, `user@example.com`,
//! …) so that two snapshots of the same `sbx` version compare cleanly.
//!
//! The check also covers the files `cargo package` publishes for the library
//! ([`published_files`]). There, the public contact the maintainer declares in
//! the root manifest (`authors`, `repository`, `homepage`) is the one personal
//! data allowed ([`declared_contacts`]): it is published on purpose.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::Value;

/// Home directory written in place of the real one.
pub(crate) const HOME: &str = "/home/user";
/// User name written in place of the real one.
pub(crate) const USER: &str = "user";
/// Host name written in place of the real one.
pub(crate) const HOST: &str = "host";
/// E-mail address written in place of real ones.
pub(crate) const EMAIL: &str = "user@example.com";
/// Value written in place of host session variables and private values.
pub(crate) const MASK: &str = "<private>";

/// Prefix of the host session variables the `sbx` CLI forwards to the daemon.
const HOST_ENV_PREFIX: &str = "SBX_HOST_";

/// Home directories that are not personal: the scrubbed one, the sandbox user
/// and the documentation placeholder.
const NEUTRAL_HOMES: &[&str] = &["user", "agent", "alice"];

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}")
        .expect("valid regex")
});
static DBUS_GUID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"guid=[0-9a-f]{32}").expect("valid regex"));
static HOME_DIR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/(?:home|Users)/([A-Za-z0-9._-]+)").expect("valid regex"));

/// One replacement rule.
#[derive(Debug, Clone)]
struct Rule {
    /// What to look for.
    pattern: Regex,
    /// What to write instead.
    replacement: String,
}

/// Replaces personal data in text and JSON values.
#[derive(Debug, Clone)]
pub(crate) struct Scrubber {
    /// Rules applied in order: the most specific (longest) values first.
    rules: Vec<Rule>,
}

impl Scrubber {
    /// A scrubber for the current machine: its home directory, user name and
    /// host name, plus the values of `DOCKER_SANDBOX_AUDIT_PRIVATE` and `extra`.
    #[must_use]
    pub(crate) fn from_env(extra: &[String]) -> Self {
        let identity = Identity::current();
        let mut private: Vec<String> = std::env::var("DOCKER_SANDBOX_AUDIT_PRIVATE")
            .map(|list| list.split(',').map(str::trim).map(str::to_owned).collect())
            .unwrap_or_default();
        private.extend(extra.iter().cloned());
        Self::new(&identity, &private)
    }

    /// A scrubber for `identity` and the additional `private` values.
    #[must_use]
    pub(crate) fn new(identity: &Identity, private: &[String]) -> Self {
        let mut literal: Vec<(String, String)> = Vec::new();
        if let Some(home) = identity.home.as_deref().filter(|h| h.len() > 1) {
            literal.push((home.trim_end_matches('/').to_owned(), HOME.to_owned()));
        }
        for value in private.iter().filter(|v| v.len() > 2) {
            literal.push((value.clone(), MASK.to_owned()));
        }
        // Longest first, so that the home directory is replaced before the user
        // name it contains.
        literal.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));

        let mut rules: Vec<Rule> = literal
            .into_iter()
            .map(|(value, replacement)| Rule {
                pattern: Regex::new(&regex::escape(&value)).expect("escaped literal"),
                replacement,
            })
            .collect();
        rules.push(Rule {
            pattern: EMAIL_RE.clone(),
            replacement: EMAIL.to_owned(),
        });
        rules.push(Rule {
            pattern: DBUS_GUID_RE.clone(),
            replacement: format!("guid={}", "0".repeat(32)),
        });
        for (value, replacement) in [(&identity.user, USER), (&identity.host, HOST)] {
            if let Some(value) = value.as_deref().filter(|v| v.len() > 2) {
                rules.push(Rule {
                    pattern: word(value),
                    replacement: replacement.to_owned(),
                });
            }
        }
        Self { rules }
    }

    /// Scrubs `text`.
    #[must_use]
    pub(crate) fn text(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for rule in &self.rules {
            if rule.pattern.is_match(&out) {
                out = rule
                    .pattern
                    .replace_all(&out, rule.replacement.as_str())
                    .into_owned();
            }
        }
        out
    }

    /// Scrubs every string of `value`, object keys included, and masks the
    /// values of host session variables.
    pub(crate) fn value(&self, value: &mut Value) {
        match value {
            Value::String(s) => {
                let scrubbed = self.text(s);
                if scrubbed != *s {
                    *s = scrubbed;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.value(item)),
            Value::Object(map) => {
                let entries = std::mem::take(map);
                for (key, mut entry) in entries {
                    if key.starts_with(HOST_ENV_PREFIX) {
                        entry = Value::String(MASK.to_owned());
                    } else {
                        self.value(&mut entry);
                    }
                    map.insert(self.text(&key), entry);
                }
            }
            _ => {}
        }
    }

    /// Scrubs one line of JSON, or the raw text when the line is not JSON.
    #[must_use]
    pub(crate) fn line(&self, line: &str) -> String {
        match serde_json::from_str::<Value>(line) {
            Ok(mut value) => {
                self.value(&mut value);
                value.to_string()
            }
            Err(_) => self.text(line),
        }
    }
}

/// A case-insensitive whole-word pattern for `value`.
fn word(value: &str) -> Regex {
    Regex::new(&format!(r"(?i)\b{}\b", regex::escape(value))).expect("escaped literal")
}

/// The identity of the machine the audit runs on.
#[derive(Debug, Clone, Default)]
pub(crate) struct Identity {
    /// Home directory.
    pub home: Option<String>,
    /// User name.
    pub user: Option<String>,
    /// Host name.
    pub host: Option<String>,
}

impl Identity {
    /// Reads the identity from `HOME`, `USER` (or `LOGNAME`, or the last
    /// component of the home directory) and the kernel host name.
    #[must_use]
    pub(crate) fn current() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let kernel_host = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .or_else(|_| std::fs::read_to_string("/etc/hostname"))
            .ok()
            .map(|h| h.trim().to_owned())
            .filter(|h| !h.is_empty());
        Self::from_vars(var, kernel_host)
    }

    /// The identity that the environment variables `var` and the kernel host
    /// name `kernel_host` describe, or none on a GitHub Actions runner
    /// (`GITHUB_ACTIONS=true`): the user and host names of a throw-away
    /// runner (`runner`, …) are nobody's, and common words a document may use.
    /// E-mail addresses, home directories and session variables are still
    /// found there.
    fn from_vars(var: impl Fn(&str) -> Option<String>, kernel_host: Option<String>) -> Self {
        if var("GITHUB_ACTIONS").as_deref() == Some("true") {
            return Self::default();
        }
        let home = var("HOME");
        let user = var("USER").or_else(|| var("LOGNAME")).or_else(|| {
            home.as_deref()
                .and_then(|h| Path::new(h).file_name())
                .map(|n| n.to_string_lossy().into_owned())
        });
        let host = kernel_host.or_else(|| var("HOSTNAME"));
        Self { home, user, host }
    }
}

/// Personal data found by [`check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    /// File the data was found in.
    pub file: PathBuf,
    /// 1-based line number.
    pub line: usize,
    /// What was found, already scrubbed so that the report itself leaks nothing.
    pub what: &'static str,
}

/// Looks for personal data in every file under `paths` (directories are walked
/// recursively). Returns what was found.
///
/// # Errors
///
/// Fails if a directory cannot be listed. Files that are not valid UTF-8 are
/// read lossily.
pub(crate) fn check(paths: &[PathBuf], scrubber: &Scrubber) -> Result<Vec<Finding>> {
    check_allowing(paths, scrubber, &[])
}

/// Same as [`check`], the values of `allowed` left out of every line first:
/// the public contacts of [`declared_contacts`].
///
/// # Errors
///
/// See [`check`].
pub(crate) fn check_allowing(
    paths: &[PathBuf],
    scrubber: &Scrubber,
    allowed: &[String],
) -> Result<Vec<Finding>> {
    let mut findings = Vec::new();
    for file in files_under(paths)? {
        let bytes = std::fs::read(&file).with_context(|| file.display().to_string())?;
        let text = String::from_utf8_lossy(&bytes);
        for (index, line) in text.lines().enumerate() {
            let line = allowed
                .iter()
                .filter(|value| !value.is_empty())
                .fold(line.to_owned(), |line, value| {
                    line.replace(value.as_str(), "")
                });
            let line = line.as_str();
            let mut found = |what| {
                findings.push(Finding {
                    file: file.clone(),
                    line: index + 1,
                    what,
                });
            };
            if scrubber.text(line) != line {
                found("personal data (home, user or host name, e-mail, private value)");
            }
            if HOME_DIR_RE
                .captures_iter(line)
                .any(|c| !NEUTRAL_HOMES.contains(&&c[1]))
            {
                found("a home directory");
            }
            if line.contains(HOST_ENV_PREFIX) && !host_env_masked(line) {
                found("a host session variable");
            }
        }
    }
    Ok(findings)
}

/// The public contacts the root manifest `manifest` declares: each entry of
/// `authors`, its e-mail address alone, and the values of `repository` and
/// `homepage`.
#[must_use]
pub(crate) fn declared_contacts(manifest: &str) -> Vec<String> {
    static AUTHORS_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*authors\s*=\s*\[([^\]]*)\]").expect("valid"));
    static LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)^\s*(?:repository|homepage)\s*=\s*"([^"]+)""#).expect("valid")
    });
    static QUOTED_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""([^"]+)""#).expect("valid"));
    let mut contacts = Vec::new();
    for authors in AUTHORS_RE.captures_iter(manifest) {
        for author in QUOTED_RE.captures_iter(&authors[1]) {
            contacts.push(author[1].to_owned());
            contacts.extend(
                EMAIL_RE
                    .find_iter(&author[1])
                    .map(|m| m.as_str().to_owned()),
            );
        }
    }
    contacts.extend(LINK_RE.captures_iter(manifest).map(|c| c[1].to_owned()));
    // Longest first, so that an author entry goes before its address.
    contacts.sort_by_key(|value| std::cmp::Reverse(value.len()));
    contacts.dedup();
    contacts
}

/// The files `cargo package` publishes for the member `package` of the
/// workspace at `root`, as they are on disk: the manifest it writes stands
/// for the manifest of the package and the root one it inherits metadata
/// from, the lockfile for the root lockfile.
///
/// # Errors
///
/// Fails if `cargo package --list` cannot run or fails.
pub(crate) fn published_files(root: &Path, package: &str) -> Result<Vec<PathBuf>> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "package",
            "--list",
            "--allow-dirty",
            "--quiet",
            "-p",
            package,
        ])
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .context("running cargo package --list")?;
    if !output.status.success() {
        bail!(
            "cargo package --list: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(published_paths(
        &String::from_utf8_lossy(&output.stdout),
        &root.join(package),
        root,
    ))
}

/// Maps the entries `list` of `cargo package --list` to files on disk.
fn published_paths(list: &str, package_dir: &Path, root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in list
        .lines()
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        match entry {
            "Cargo.toml" | "Cargo.toml.orig" => {
                files.push(package_dir.join("Cargo.toml"));
                files.push(root.join("Cargo.toml"));
            }
            "Cargo.lock" => files.push(root.join("Cargo.lock")),
            _ => files.push(package_dir.join(entry)),
        }
    }
    files.sort();
    files.dedup();
    files
}

/// Whether every `SBX_HOST_*` value on `line` is masked.
fn host_env_masked(line: &str) -> bool {
    static HOST_ENV_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""SBX_HOST_[A-Z0-9_]*"\s*:\s*"([^"]*)""#).expect("valid"));
    HOST_ENV_RE.captures_iter(line).all(|c| &c[1] == MASK)
}

/// Every regular file under `paths`, sorted.
fn files_under(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending: Vec<PathBuf> = paths.to_vec();
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            for entry in
                std::fs::read_dir(&path).with_context(|| format!("listing {}", path.display()))?
            {
                pending.push(entry?.path());
            }
        } else if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scrubber() -> Scrubber {
        Scrubber::new(
            &Identity {
                home: Some("/home/jdoe".into()),
                user: Some("jdoe".into()),
                host: Some("WORKSTATION-7".into()),
            },
            &["dockerjd".into()],
        )
    }

    #[test]
    fn identity_and_contacts_are_replaced() {
        let s = scrubber();
        assert_eq!(
            s.text(
                "/home/jdoe/workspace/jdoe/repo on workstation-7 by jdoe.smith@mail.example.org"
            ),
            "/home/user/workspace/user/repo on host by user@example.com"
        );
        assert_eq!(s.text("logged in as dockerjd"), "logged in as <private>");
        assert_eq!(
            s.text("docker.io/sbx/shell@sha256:abc"),
            "docker.io/sbx/shell@sha256:abc",
            "image digests are not e-mail addresses"
        );
        assert_eq!(s.text("jdoes"), "jdoes", "only whole words are replaced");
    }

    #[test]
    fn host_session_variables_are_masked() {
        let s = scrubber();
        let mut value = json!({
            "env": {
                "SBX_HOST_DISPLAY": ":0",
                "SBX_HOST_DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/user/1000/bus,guid=4d758074f7ad38eed9bfceef6b00ffb4",
                "COLORTERM": "truecolor"
            },
            "/home/jdoe/key": "guid=4d758074f7ad38eed9bfceef6b00ffb4"
        });
        s.value(&mut value);
        assert_eq!(
            value,
            json!({
                "env": { "SBX_HOST_DISPLAY": MASK, "SBX_HOST_DBUS_SESSION_BUS_ADDRESS": MASK, "COLORTERM": "truecolor" },
                "/home/user/key": format!("guid={}", "0".repeat(32))
            })
        );
    }

    #[test]
    fn lines_that_are_not_json_are_scrubbed_as_text() {
        assert_eq!(scrubber().line("cwd=/home/jdoe/x"), "cwd=/home/user/x");
        assert_eq!(
            scrubber().line(r#"{"path":"/home/jdoe"}"#),
            r#"{"path":"/home/user"}"#
        );
    }

    #[test]
    fn the_declared_contact_is_the_one_personal_data_allowed() {
        let manifest = "[workspace.package]\n\
                        authors    = [\"Jane Doe <jane@doe.example.org>\"]\n\
                        repository = \"https://github.com/jdoe/project\"\n";
        let allowed = declared_contacts(manifest);
        assert_eq!(
            allowed,
            [
                "Jane Doe <jane@doe.example.org>",
                "https://github.com/jdoe/project",
                "jane@doe.example.org"
            ]
        );

        let dir = std::env::temp_dir().join(format!(
            "docker-sandbox-audit-contacts-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "authors = [\"Jane Doe <jane@doe.example.org>\"]\n\
             repository = \"https://github.com/jdoe/project\"\n\
             contact = \"jane@doe.example.org\"\n\
             other = \"someone@mail.example.org\"\n\
             user = \"jdoe\"\n",
        )
        .unwrap();
        let findings = check_allowing(std::slice::from_ref(&dir), &scrubber(), &allowed).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let lines: Vec<usize> = findings.iter().map(|f| f.line).collect();
        assert_eq!(
            lines,
            [4, 5],
            "another address and the user name are still found"
        );
    }

    #[test]
    fn a_github_actions_runner_has_no_personal_identity() {
        let env = |runner: bool| {
            move |name: &str| match name {
                "GITHUB_ACTIONS" if runner => Some("true".to_owned()),
                "HOME" => Some("/home/runner".to_owned()),
                "USER" => Some("runner".to_owned()),
                _ => None,
            }
        };
        let on_runner = Identity::from_vars(env(true), Some("fv-az42".into()));
        assert!(on_runner.home.is_none() && on_runner.user.is_none() && on_runner.host.is_none());
        let elsewhere = Identity::from_vars(env(false), Some("fv-az42".into()));
        assert_eq!(elsewhere.user.as_deref(), Some("runner"));
        assert_eq!(elsewhere.host.as_deref(), Some("fv-az42"));
        // The other rules still apply on a runner.
        let scrubber = Scrubber::new(&on_runner, &[]);
        assert_eq!(
            scrubber.text("Building a runner on it"),
            "Building a runner on it"
        );
        assert_eq!(scrubber.text("jane@doe.example.org"), EMAIL);
    }

    #[test]
    fn published_entries_map_to_their_sources() {
        let list = "Cargo.lock\nCargo.toml\nCargo.toml.orig\nREADME.md\nsrc/lib.rs\n";
        let (package, root) = (Path::new("/w/lib"), Path::new("/w"));
        assert_eq!(
            published_paths(list, package, root),
            [
                PathBuf::from("/w/Cargo.lock"),
                PathBuf::from("/w/Cargo.toml"),
                PathBuf::from("/w/lib/Cargo.toml"),
                PathBuf::from("/w/lib/README.md"),
                PathBuf::from("/w/lib/src/lib.rs"),
            ]
        );
    }

    #[test]
    fn the_check_flags_foreign_home_directories_and_unmasked_variables() {
        let dir = std::env::temp_dir().join(format!(
            "docker-sandbox-audit-privacy-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.jsonl"),
            "{\"p\":\"/home/user/x\",\"q\":\"/home/agent/y\"}\n{\"p\":\"/home/someone/x\"}\n{\"SBX_HOST_DISPLAY\":\":0\"}\n",
        )
        .unwrap();
        let findings = check(std::slice::from_ref(&dir), &scrubber()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let lines: Vec<(usize, &str)> = findings.iter().map(|f| (f.line, f.what)).collect();
        assert_eq!(
            lines,
            [(2, "a home directory"), (3, "a host session variable")]
        );
    }
}
