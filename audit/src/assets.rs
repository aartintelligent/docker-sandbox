//! Files the audit needs at run time, embedded in the binary.
//!
//! The sources live in `audit/assets/`:
//!
//! | Path | Content |
//! |---|---|
//! | `suites/*.json` | Scenario suites, replayed in [`SUITES`] order |
//! | `kits/audit-mixin-v3/` | Test mixin used by the `kits` suite and the live tests |
//! | `kits/audit-mixin-{net,net-install,cred,cred-required,mcp}/` | Test mixins of the `kits` suite: network rules, credentials, MCP gateway |
//! | `kits/audit-mixin-json{,-b,…,-j}/` | Test mixins written in the JSON subset of YAML, forged by `kit-forge` |
//! | `fixtures/` | Input files of the scenarios (`{fixtures}`) |
//! | `notes.md` | Hand-written observations, inserted into `API.md` |
//! | `probe-routes.txt` | Additional routes for `probe` |
//!
//! Before a run, [`WorkDir::prepare`] writes the fixtures and kits into a
//! fixed directory under the system temporary directory, so that the paths the
//! CLI sends to the daemon are the same on every machine and every run.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::scenarios::Suite;

/// The suites, in the order the audit replays them. Each suite removes the
/// sandboxes it creates.
pub(crate) const SUITES: &[(&str, &str)] = &[
    ("lifecycle", include_str!("../assets/suites/lifecycle.json")),
    ("coverage", include_str!("../assets/suites/coverage.json")),
    ("templates", include_str!("../assets/suites/templates.json")),
    ("sessions", include_str!("../assets/suites/sessions.json")),
    ("kits", include_str!("../assets/suites/kits.json")),
    ("errors", include_str!("../assets/suites/errors.json")),
    ("agents", include_str!("../assets/suites/agents.json")),
];

/// Hand-written observations inserted into `API.md`.
pub(crate) const NOTES: &str = include_str!("../assets/notes.md");

/// Additional routes for `probe`, one per line.
pub(crate) const PROBE_ROUTES: &str = include_str!("../assets/probe-routes.txt");

/// Scenario input files, relative to the fixtures directory.
const FIXTURES: &[(&str, &str)] = &[
    ("probe.txt", include_str!("../assets/fixtures/probe.txt")),
    (
        "sbxenv.yaml",
        include_str!("../assets/fixtures/sbxenv.yaml"),
    ),
    (
        "mountdir/hello.txt",
        include_str!("../assets/fixtures/mountdir/hello.txt"),
    ),
    (
        "docs/readme.txt",
        include_str!("../assets/fixtures/docs/readme.txt"),
    ),
    (
        "data/values.txt",
        include_str!("../assets/fixtures/data/values.txt"),
    ),
];

/// Kit sources, relative to the kits directory.
const KITS: &[(&str, &str)] = &[
    (
        "audit-mixin-v3/spec.yaml",
        include_str!("../assets/kits/audit-mixin-v3/spec.yaml"),
    ),
    (
        "audit-mixin-net/spec.yaml",
        include_str!("../assets/kits/audit-mixin-net/spec.yaml"),
    ),
    (
        "audit-mixin-net-install/spec.yaml",
        include_str!("../assets/kits/audit-mixin-net-install/spec.yaml"),
    ),
    (
        "audit-mixin-cred/spec.yaml",
        include_str!("../assets/kits/audit-mixin-cred/spec.yaml"),
    ),
    (
        "audit-mixin-cred-required/spec.yaml",
        include_str!("../assets/kits/audit-mixin-cred-required/spec.yaml"),
    ),
    (
        "audit-mixin-mcp/spec.yaml",
        include_str!("../assets/kits/audit-mixin-mcp/spec.yaml"),
    ),
    (
        "audit-mixin-json/spec.yaml",
        include_str!("../assets/kits/audit-mixin-json/spec.yaml"),
    ),
    (
        "audit-mixin-json-b/spec.yaml",
        include_str!("../assets/kits/audit-mixin-json-b/spec.yaml"),
    ),
    (
        "audit-mixin-json-c/spec.yaml",
        include_str!("../assets/kits/audit-mixin-json-c/spec.yaml"),
    ),
    (
        "audit-mixin-json-d/spec.yaml",
        include_str!("../assets/kits/audit-mixin-json-d/spec.yaml"),
    ),
    (
        "audit-mixin-json-e/spec.yaml",
        include_str!("../assets/kits/audit-mixin-json-e/spec.yaml"),
    ),
    (
        "audit-mixin-json-f/spec.yaml",
        include_str!("../assets/kits/audit-mixin-json-f/spec.yaml"),
    ),
    (
        "audit-mixin-json-g/spec.yaml",
        include_str!("../assets/kits/audit-mixin-json-g/spec.yaml"),
    ),
    (
        "audit-mixin-json-h/spec.yaml",
        include_str!("../assets/kits/audit-mixin-json-h/spec.yaml"),
    ),
    (
        "audit-mixin-json-i/spec.yaml",
        include_str!("../assets/kits/audit-mixin-json-i/spec.yaml"),
    ),
    (
        "audit-mixin-json-j/spec.yaml",
        include_str!("../assets/kits/audit-mixin-json-j/spec.yaml"),
    ),
];

/// Parses the embedded suite `name`.
///
/// # Errors
///
/// Fails if there is no such suite or if it does not parse.
pub(crate) fn suite(name: &str) -> Result<Suite> {
    let (_, text) = SUITES.iter().find(|(n, _)| *n == name).with_context(|| {
        let names: Vec<&str> = SUITES.iter().map(|(n, _)| *n).collect();
        format!("unknown suite {name:?} (known: {})", names.join(", "))
    })?;
    serde_json::from_str(text).with_context(|| format!("parsing the {name} suite"))
}

/// Parses a suite file.
///
/// # Errors
///
/// Fails if the file cannot be read or does not parse.
pub(crate) fn suite_file(path: &Path) -> Result<Suite> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The scratch directory of a run: scenario inputs, kits, spy socket and the
/// files the scenarios write.
#[derive(Debug, Clone)]
pub(crate) struct WorkDir {
    /// Root of the directory (`{work}`).
    pub root: PathBuf,
}

impl WorkDir {
    /// The fixed location of the scratch directory.
    #[must_use]
    pub(crate) fn default_root() -> PathBuf {
        std::env::temp_dir().join("docker-sandbox-audit-work")
    }

    /// Empties `root`, then writes the fixtures and kits into it.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be cleared or written.
    pub(crate) fn prepare(root: &Path) -> Result<Self> {
        if root.exists() {
            std::fs::remove_dir_all(root)
                .with_context(|| format!("clearing {}", root.display()))?;
        }
        let work = Self {
            root: root.to_path_buf(),
        };
        for (dir, files) in [(work.fixtures(), FIXTURES), (work.kits(), KITS)] {
            for (relative, content) in files {
                let path = dir.join(relative);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, content).with_context(|| path.display().to_string())?;
            }
        }
        Ok(work)
    }

    /// Scenario input files (`{fixtures}`).
    #[must_use]
    pub(crate) fn fixtures(&self) -> PathBuf {
        self.root.join("fixtures")
    }

    /// Kit sources (`{kits}`).
    #[must_use]
    pub(crate) fn kits(&self) -> PathBuf {
        self.root.join("kits")
    }

    /// Socket of the spy, when it does not replace the real one.
    #[must_use]
    pub(crate) fn spy_socket(&self) -> PathBuf {
        self.root.join("spy.sock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_suite_parses_and_names_its_scenarios_uniquely() {
        for (name, _) in SUITES {
            let suite = suite(name).unwrap();
            assert_eq!(suite.name, *name);
            let mut names: Vec<&str> = suite.scenarios.iter().map(|s| s.name.as_str()).collect();
            names.sort_unstable();
            let count = names.len();
            names.dedup();
            assert_eq!(names.len(), count, "duplicate scenario names in {name}");
        }
    }

    #[test]
    fn the_work_dir_holds_fixtures_and_kits() {
        let root = std::env::temp_dir().join(format!(
            "docker-sandbox-audit-assets-{}",
            std::process::id()
        ));
        let work = WorkDir::prepare(&root).unwrap();
        assert!(work.fixtures().join("mountdir/hello.txt").is_file());
        assert!(work.kits().join("audit-mixin-v3/spec.yaml").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
}
