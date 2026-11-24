//! Turns a spy capture into a route catalog and a scenario
//! matrix.
//!
//! [`run`] reads a capture (`capture.jsonl`), pairs each request with its
//! response, folds concrete paths into route templates (see
//! [`template_path`]) and writes two files:
//!
//! - `catalog.json`: one [`RouteEntry`] per distinct method and route;
//! - `matrix.md`: a Markdown table mapping each scenario to the socket routes
//!   it used, followed by the catalog rendered as a table.

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::BufRead;
use std::path::Path;
use std::sync::LazyLock;

/// Identity of a route: HTTP method plus templated path.
///
/// Ordering is by method, then by route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RouteKey {
    /// HTTP method, or `?` when the capture does not record it.
    pub method: String,
    /// Templated path produced by [`template_path`].
    pub route: String,
}

/// Everything observed for one route in a capture.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RouteEntry {
    /// Method and templated path of the route.
    pub key: RouteKey,
    /// Tags (scenario names) of the requests that hit this route.
    pub scenarios: BTreeSet<String>,
    /// Distinct response status codes observed.
    pub statuses: BTreeSet<u16>,
    /// Number of requests observed.
    pub count: u32,
    /// Body of one request observed on this route, if it had one.
    pub sample_request: Option<Value>,
    /// Body of the response to that same request, if any.
    pub sample_response: Option<Value>,
    /// `true` when at least one connection that carried this route switched to
    /// raw streaming (connection upgrade or hijack, as used by `exec` and
    /// `attach`).
    pub hijacked: bool,
}

/// Route catalog, serialized as `catalog.json`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Catalog {
    /// Entries sorted by [`RouteKey`].
    pub routes: Vec<RouteEntry>,
}

/// Turns a concrete request path into a route template.
///
/// The following substitutions are applied, in this order:
///
/// - every occurrence of a name from `names` (sandbox names) becomes
///   `{sandbox}`;
/// - UUIDs become `{id}`;
/// - path segments made of 12 to 64 lowercase hexadecimal digits become
///   `{hash}`;
/// - query parameter values become `…`, so that only the keys remain.
///
/// For example, with `names` set to `["audit-a"]`, the path
/// `/sandbox/audit-a/files?path=/etc/hostname` becomes
/// `/sandbox/{sandbox}/files?path=…`.
pub(crate) fn template_path(path: &str, names: &[String]) -> String {
    static UUID: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
            .expect("valid regex")
    });
    static HASH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[0-9a-f]{12,64}$").expect("valid regex"));

    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    // Whole segments only: a substring replacement would turn `audit-ab` into
    // `{sandbox}b` when `audit-a` is a known name.
    let templated: Vec<&str> = path
        .split('/')
        .map(|segment| {
            if names.iter().any(|name| name == segment) {
                "{sandbox}"
            } else if UUID.is_match(segment) {
                "{id}"
            } else if HASH.is_match(segment) {
                "{hash}"
            } else {
                segment
            }
        })
        .collect();
    let path = templated.join("/");
    if query.is_empty() {
        path
    } else {
        let keys: Vec<String> = query
            .split('&')
            .map(|kv| format!("{}=…", kv.split('=').next().unwrap_or("")))
            .collect();
        format!("{path}?{}", keys.join("&"))
    }
}

/// A spy capture, indexed for the catalog.
#[derive(Default)]
struct CaptureIndex {
    /// Exchanges in capture order: each request with the response that
    /// followed it on the same `(conn, seq)`, if any.
    exchanges: Vec<(Value, Option<Value>)>,
    /// Result of each scenario, by tag.
    scenario_results: BTreeMap<String, Value>,
    /// Connections that carried raw (upgraded or hijacked) traffic.
    hijacked_conns: BTreeSet<u64>,
}

impl CaptureIndex {
    /// Reads the capture at `path`. Lines that are not valid JSON are skipped.
    fn read(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let mut index = Self::default();
        // Connection ids restart with every spy run, so a `(conn, seq)` key may
        // occur several times in an appended capture: a response completes the
        // latest request with the same key.
        let mut pending: HashMap<(u64, u64), usize> = HashMap::new();
        for line in std::io::BufReader::new(file).lines() {
            let Ok(event) = serde_json::from_str::<Value>(&line?) else {
                continue;
            };
            let conn = event["conn"].as_u64().unwrap_or(0);
            let seq = event["seq"].as_u64().unwrap_or(0);
            match event["kind"].as_str() {
                Some("request") => {
                    pending.insert((conn, seq), index.exchanges.len());
                    index.exchanges.push((event, None));
                }
                Some("response") => {
                    if let Some(position) = pending.remove(&(conn, seq)) {
                        index.exchanges[position].1 = Some(event);
                    }
                }
                Some("raw") => {
                    index.hijacked_conns.insert(conn);
                }
                Some("scenario_result") => {
                    if let Some(tag) = event["tag"].as_str() {
                        index
                            .scenario_results
                            .insert(tag.to_string(), event["result"].clone());
                    }
                }
                _ => {}
            }
        }
        Ok(index)
    }
}

/// Builds the route catalog from the capture at `capture` and writes
/// `catalog.json` and `matrix.md` into `out`.
///
/// `names` lists the sandbox names to fold into `{sandbox}` (see
/// [`template_path`]). Capture lines that are not valid JSON are skipped. The
/// matrix only lists scenarios that logged a `scenario_result` event, whereas
/// the catalog covers every request in the capture.
///
/// # Errors
///
/// Fails if the capture cannot be read, or if the output directory or files
/// cannot be written.
pub(crate) fn run(capture: &Path, out: &Path, names: &[String]) -> Result<Catalog> {
    let index = CaptureIndex::read(capture)?;

    // Aggregate requests per route, and collect the routes used by each
    // scenario for the matrix.
    let mut routes: BTreeMap<RouteKey, RouteEntry> = BTreeMap::new();
    let mut by_scenario: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (request, response) in &index.exchanges {
        let key = RouteKey {
            method: request["method"].as_str().unwrap_or("?").into(),
            route: template_path(request["path"].as_str().unwrap_or("?"), names),
        };
        let tag = request["tag"].as_str().unwrap_or("?").to_string();
        let entry = routes.entry(key.clone()).or_insert_with(|| RouteEntry {
            key: key.clone(),
            scenarios: BTreeSet::new(),
            statuses: BTreeSet::new(),
            count: 0,
            sample_request: None,
            sample_response: None,
            hijacked: false,
        });
        // Samples come from the first exchange that has them, in capture order.
        if entry.sample_request.is_none() {
            entry.sample_request = request.get("body").cloned();
        }
        if entry.sample_response.is_none() {
            entry.sample_response = response.as_ref().and_then(|r| r.get("body").cloned());
        }
        entry.count += 1;
        entry.scenarios.insert(tag.clone());
        let status = response.as_ref().and_then(|r| r["status"].as_u64());
        if let Some(status) = status.and_then(|s| u16::try_from(s).ok()) {
            entry.statuses.insert(status);
        }
        let conn = request["conn"].as_u64().unwrap_or(0);
        entry.hijacked |= index.hijacked_conns.contains(&conn);
        by_scenario
            .entry(tag)
            .or_default()
            .insert(format!("{} {}", key.method, key.route));
    }

    let catalog = Catalog {
        routes: routes.into_values().collect(),
    };
    std::fs::create_dir_all(out)?;
    std::fs::write(
        out.join("catalog.json"),
        serde_json::to_vec_pretty(&catalog)?,
    )?;
    let matrix = matrix_markdown(&catalog, &index.scenario_results, &by_scenario)?;
    std::fs::write(out.join("matrix.md"), matrix)?;
    Ok(catalog)
}

/// Renders `matrix.md`: the routes used by each scenario, meant to be copied
/// into the results section of the audit report, then the catalog.
fn matrix_markdown(
    catalog: &Catalog,
    scenario_results: &BTreeMap<String, Value>,
    by_scenario: &BTreeMap<String, BTreeSet<String>>,
) -> Result<String> {
    let mut md = String::from(
        "| Scenario | Exit | Conns | Observed socket routes | Verdict (to fill in) |\n|---|---|---|---|---|\n",
    );
    for (tag, result) in scenario_results {
        let routes = by_scenario.get(tag).map_or_else(
            || "—".to_owned(),
            |routes| routes.iter().cloned().collect::<Vec<_>>().join("<br>"),
        );
        writeln!(
            md,
            "| `{tag}` | {} | {} | {routes} | |",
            result["exit_code"], result["connections"]
        )?;
    }
    md.push_str("\n## Catalog\n\n| Method | Route | Statuses | Scenarios | Hijacked |\n|---|---|---|---|---|\n");
    for entry in &catalog.routes {
        writeln!(
            md,
            "| {} | `{}` | {:?} | {} | {} |",
            entry.key.method,
            entry.key.route,
            entry.statuses,
            entry
                .scenarios
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            if entry.hijacked { "yes" } else { "" }
        )?;
    }
    Ok(md)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_templated_by_whole_segments() {
        let names = vec!["audit-a".to_owned()];
        assert_eq!(
            template_path("/sandbox/audit-a/files?path=/etc/hostname", &names),
            "/sandbox/{sandbox}/files?path=…"
        );
        assert_eq!(
            template_path("/sandbox/audit-ab", &names),
            "/sandbox/audit-ab"
        );
        assert_eq!(
            template_path("/x/0123456789ab/fedcba987654", &names),
            "/x/{hash}/{hash}"
        );
        assert_eq!(
            template_path(
                "/exec/123e4567-e89b-12d3-a456-426614174000/resize?h=1&w=2",
                &names
            ),
            "/exec/{id}/resize?h=…&w=…"
        );
    }
}
