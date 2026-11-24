//! `fixtures` — export captured exchanges as test fixtures for `docker-sandbox`.
//!
//! One JSON file is written per (method, route, status), holding the first
//! captured exchange of that kind:
//!
//! ```json
//! {
//!   "method": "GET",
//!   "route": "/sandbox/{name}",
//!   "status": 200,
//!   "request": null,
//!   "response": { "...": "..." }
//! }
//! ```
//!
//! The output directory holds exactly the fixtures of the snapshot: a fixture
//! of an exchange the snapshot no longer has is removed, so that a route or a
//! status that disappears with an `sbx` upgrade shows in the diff.
//!
//! Fixtures are committed, so every one goes through the
//! [`Scrubber`] again before it is written (captures
//! are already scrubbed when recorded). Secret values are redacted by the spy,
//! and routes describing the host rather than the API are skipped (see
//! `EXCLUDED_ROUTES`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::privacy::Scrubber;
use crate::spec::{load_exchanges, load_patterns, match_pattern};

/// Routes whose payloads describe the host or past activity rather than the API
/// contract (process lists, VM metrics, network history, token metadata). They
/// are never exported.
const EXCLUDED_ROUTES: &[&str] = &[
    "/daemon/diagnostics",
    "/daemon/debug/state",
    "/sandbox/{name}/debug/vm",
    "/policy/network/log",
];

/// Options of the `fixtures` command.
pub(crate) struct FixturesOpts<'a> {
    /// Capture files to read.
    pub captures: &'a [PathBuf],
    /// Route inventory (binary scan + `OPTIONS`).
    pub routes: Option<&'a Path>,
    /// Router patterns extracted from the daemon log.
    pub router_log: Option<&'a Path>,
    /// Output directory.
    pub out: &'a Path,
    /// Removes personal data from the fixtures.
    pub scrubber: &'a Scrubber,
}

/// What an export changed in the output directory.
pub(crate) struct Exported {
    /// Number of fixtures written.
    pub written: usize,
    /// Fixtures removed because the snapshot no longer has their exchange.
    pub removed: Vec<String>,
}

/// Exports one fixture per (method, route, status) and removes the fixtures
/// the snapshot no longer has.
pub(crate) fn run(opts: &FixturesOpts<'_>) -> Result<Exported> {
    let patterns = load_patterns(opts.routes, opts.router_log)?;
    let exchanges = load_exchanges(opts.captures)?;

    let mut selected: BTreeMap<String, Value> = BTreeMap::new();
    for exchange in &exchanges {
        let (Some(status), Some(response)) = (exchange.status, &exchange.resp_body) else {
            continue;
        };
        if exchange.method == "OPTIONS" || is_binary(response) {
            continue;
        }
        let Some(pattern) = match_pattern(&patterns, &exchange.path) else {
            continue;
        };
        let route = pattern.openapi_path();
        if EXCLUDED_ROUTES.contains(&route.as_str()) {
            continue;
        }
        let file_name = format!(
            "{}_{}_{status}.json",
            exchange.method.to_lowercase(),
            slug(&route)
        );
        selected.entry(file_name).or_insert_with(|| {
            json!({
                "method": exchange.method,
                "route": route,
                "status": status,
                "request": exchange.req_body.as_ref().filter(|b| !is_binary(b)),
                "response": response,
            })
        });
    }

    std::fs::create_dir_all(opts.out)
        .with_context(|| format!("creating {}", opts.out.display()))?;
    let mut removed = Vec::new();
    for entry in
        std::fs::read_dir(opts.out).with_context(|| format!("listing {}", opts.out.display()))?
    {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if path.extension().is_some_and(|e| e == "json") && !selected.contains_key(name) {
            removed.push(name.to_owned());
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    removed.sort();

    let written = selected.len();
    for (file_name, mut fixture) in selected {
        opts.scrubber.value(&mut fixture);
        let mut bytes = serde_json::to_vec_pretty(&fixture)?;
        bytes.push(b'\n');
        let path = opts.out.join(&file_name);
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(Exported { written, removed })
}

/// Whether a captured body is binary or a raw archive rather than JSON.
fn is_binary(body: &Value) -> bool {
    body.get("binary_preview").is_some() || body.as_str().is_some_and(|s| s.contains('\u{0}'))
}

/// `/sandbox/{name}/exec/{exec_id}` → `sandbox_name_exec_exec_id`.
fn slug(route: &str) -> String {
    let slug: String = route
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    slug.split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_file_safe() {
        assert_eq!(
            slug("/sandbox/{name}/exec/{exec_id}"),
            "sandbox_name_exec_exec_id"
        );
        assert_eq!(slug("/daemon/disk-usage"), "daemon_disk_usage");
    }
}
