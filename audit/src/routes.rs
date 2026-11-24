//! Router inventory: every route the daemon answers, including those the CLI
//! never calls.
//!
//! The daemon is the `sbx` binary itself (`sbx daemon start`). Its routes are
//! string literals in the binary, so [`inventory`]:
//!
//! 1. extracts every path-like string (`/sandbox/:name/exec/attach`, …) and the
//!    prefixes of each one at segment boundaries, because the linker packs
//!    literals without separators;
//! 2. sends `OPTIONS` for each candidate, parameters replaced by a dummy value,
//!    and keeps those the router answers with an `Allow` header;
//! 3. repeats with the suffixes that start at a confirmed top-level segment, to
//!    recover routes glued after another literal, and with every prefix of the
//!    segment that follows a known route node, to recover routes whose literal
//!    is glued to the next one (`/sandbox/:name/swap-containerunknown`);
//! 4. reduces every confirmed spelling to the pattern the router matched: a
//!    segment is a parameter when replacing it keeps the same answer, and a
//!    parameter is a catch-all when extra segments after it keep it too
//!    (`/sandbox/bridge.go` is `/sandbox/:name`, `/daemon/settings/a/b` is
//!    `/daemon/settings/:key`).
//!
//! `OPTIONS` is answered by the router without calling the handler, so the probe
//! has no side effect. Routes listed in [`NEVER_PROBE`](crate::probe::NEVER_PROBE)
//! are never sent at all.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::client::SockClient;

/// Value given to route parameters (`:name`, `:exec_id`, …) when probing.
const DUMMY: &str = "zz-audit";
/// Value replacing a static segment to test whether it is a parameter.
const PROBE: &str = "zz-audit-probe";
/// Requests in flight at the same time.
const CONCURRENCY: usize = 32;
/// Rounds of the glued-literal expansion.
const EXPANSION_ROUNDS: usize = 2;

/// Result of [`inventory`], written as `routes.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Inventory {
    /// Route pattern, as spelled in the binary, → value of the `Allow` header.
    pub routes: BTreeMap<String, String>,
    /// Number of distinct candidates extracted from the binary.
    pub candidates: usize,
    /// Number of `OPTIONS` requests sent.
    pub requests: usize,
    /// Candidates never sent because they designate a destructive route.
    pub skipped_danger: BTreeSet<String>,
}

/// Builds the router inventory of the daemon listening on `socket`, whose
/// binary is `binary`.
///
/// # Errors
///
/// Fails if the binary cannot be read or the socket cannot be reached.
pub(crate) async fn inventory(socket: &Path, binary: &Path) -> Result<Inventory> {
    let bytes = std::fs::read(binary).with_context(|| format!("reading {}", binary.display()))?;
    let literals = extract(&bytes);
    drop(bytes);
    let client = SockClient::new(socket);
    client
        .request("GET", "/daemon/health")
        .await
        .context("the daemon does not answer")?;

    let mut inventory = Inventory::default();
    let mut probed: BTreeSet<String> = BTreeSet::new();
    let first: BTreeSet<String> = literals.iter().flat_map(|l| prefixes(l)).collect();
    inventory.candidates = first.len();
    probe_all(&client, first, &mut probed, &mut inventory).await;

    // Routes glued after another literal start with a known top-level segment.
    let roots: BTreeSet<String> = inventory
        .routes
        .keys()
        .filter_map(|r| r.split('/').nth(1).map(str::to_owned))
        .collect();
    let second: BTreeSet<String> = literals
        .iter()
        .flat_map(|l| suffixes(l, &roots))
        .flat_map(|s| prefixes(&s))
        .filter(|c| !probed.contains(c))
        .collect();
    inventory.candidates += second.len();
    probe_all(&client, second, &mut probed, &mut inventory).await;

    // A literal glued to the next one ends in the middle of a segment: under
    // every known route node, try each prefix of the segment that follows.
    for _ in 0..EXPANSION_ROUNDS {
        let patterns = normalize(&client, &inventory.routes, &mut inventory.requests).await;
        let nodes: BTreeSet<Vec<Segment>> = patterns.keys().flat_map(|p| node_shapes(p)).collect();
        let glued: BTreeSet<String> = literals
            .iter()
            .flat_map(|l| glued_children(l, &nodes))
            .filter(|c| !probed.contains(c))
            .collect();
        if glued.is_empty() {
            break;
        }
        inventory.candidates += glued.len();
        probe_all(&client, glued, &mut probed, &mut inventory).await;
    }

    let confirmed = std::mem::take(&mut inventory.routes);
    inventory.routes = normalize(&client, &confirmed, &mut inventory.requests).await;
    Ok(inventory)
}

/// A segment of a router pattern.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Segment {
    /// A literal segment.
    Static(String),
    /// A parameter, with the name spelled in the binary when there is one.
    Param(Option<String>),
}

/// Reduces confirmed spellings to router patterns, one per shape.
async fn normalize(
    client: &SockClient,
    confirmed: &BTreeMap<String, String>,
    requests: &mut usize,
) -> BTreeMap<String, String> {
    // Shape (parameters without names) → (best spelling, Allow).
    let mut by_shape: BTreeMap<Vec<Segment>, (Vec<Segment>, String)> = BTreeMap::new();
    for (spelling, allow) in confirmed {
        let pattern = pattern_of(client, spelling, allow, requests).await;
        let shape: Vec<Segment> = pattern
            .iter()
            .map(|s| match s {
                Segment::Param(_) => Segment::Param(None),
                Segment::Static(v) => Segment::Static(v.clone()),
            })
            .collect();
        let entry = by_shape
            .entry(shape)
            .or_insert_with(|| (pattern.clone(), allow.clone()));
        if names_score(&pattern) > names_score(&entry.0) {
            entry.0 = pattern;
        }
    }
    let patterns: Vec<(Vec<Segment>, String)> = by_shape.into_values().collect();
    let named = |prefix: &[Segment]| -> Option<String> {
        let k = prefix.len();
        patterns.iter().find_map(|(other, _)| {
            let same_prefix =
                other.len() > k && other[..k].iter().zip(prefix).all(|(a, b)| same_kind(a, b));
            match other.get(k) {
                Some(Segment::Param(Some(name))) if same_prefix => Some(name.clone()),
                _ => None,
            }
        })
    };
    // A parameter only spelled by a format string takes the name its node has
    // in another pattern (`/sandbox/%s/start` → `/sandbox/:name/start`).
    patterns
        .iter()
        .map(|(pattern, allow)| {
            let mut pattern = pattern.clone();
            for k in 0..pattern.len() {
                if pattern[k] == Segment::Param(None)
                    && let Some(name) = named(&pattern[..k])
                {
                    pattern[k] = Segment::Param(Some(name));
                }
            }
            (render(&pattern), allow.clone())
        })
        .collect()
}

/// Whether two segments are both parameters or the same literal.
fn same_kind(a: &Segment, b: &Segment) -> bool {
    match (a, b) {
        (Segment::Param(_), Segment::Param(_)) => true,
        (Segment::Static(x), Segment::Static(y)) => x == y,
        _ => false,
    }
}

/// The router pattern of a confirmed spelling.
async fn pattern_of(
    client: &SockClient,
    spelling: &str,
    allow: &str,
    requests: &mut usize,
) -> Vec<Segment> {
    let mut pattern: Vec<Segment> = spelling
        .split('/')
        .skip(1)
        .map(|s| match s.strip_prefix(':') {
            Some("") => Segment::Param(None),
            Some(name) => Segment::Param(Some(name.to_owned())),
            None => Segment::Static(s.to_owned()),
        })
        .collect();
    // The first segment selects the API area and is never a parameter.
    for i in 1..pattern.len() {
        if matches!(pattern[i], Segment::Param(_)) {
            continue;
        }
        let mut variant = pattern.clone();
        variant[i] = Segment::Static(PROBE.to_owned());
        *requests += 1;
        if same_answer(client, &concrete_of(&variant), allow).await {
            pattern[i] = Segment::Param(None);
        }
    }
    // A parameter followed by anything is a catch-all: keep it last.
    for k in 1..pattern.len() {
        if !matches!(pattern[k], Segment::Param(_)) {
            continue;
        }
        let deeper = format!("{}/{PROBE}/{PROBE}", concrete_of(&pattern[..=k]));
        *requests += 1;
        if same_answer(client, &deeper, allow).await {
            pattern.truncate(k + 1);
            break;
        }
    }
    pattern
}

/// Whether `OPTIONS path` is answered with exactly `allow`.
async fn same_answer(client: &SockClient, path: &str, allow: &str) -> bool {
    if crate::probe::is_forbidden(path) {
        return false;
    }
    let Ok(response) = client.request("OPTIONS", path).await else {
        return false;
    };
    response.status.as_u16() != 404
        && response
            .headers
            .get("allow")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == allow)
}

/// How well a pattern names its parameters: named parameters count, shorter
/// names are better (names glued to the next literal are long).
fn names_score(pattern: &[Segment]) -> (usize, std::cmp::Reverse<usize>) {
    let names: Vec<&String> = pattern
        .iter()
        .filter_map(|s| match s {
            Segment::Param(Some(name)) => Some(name),
            Segment::Param(None) | Segment::Static(_) => None,
        })
        .collect();
    (
        names.len(),
        std::cmp::Reverse(names.iter().map(|n| n.len()).sum()),
    )
}

/// `/a/:name/c`, with unnamed parameters numbered `:param`, `:param2`, ….
fn render(pattern: &[Segment]) -> String {
    let mut unnamed = 0;
    let mut out = String::new();
    for segment in pattern {
        out.push('/');
        match segment {
            Segment::Static(value) => out.push_str(value),
            Segment::Param(Some(name)) => {
                out.push(':');
                out.push_str(name);
            }
            Segment::Param(None) => {
                unnamed += 1;
                out.push_str(":param");
                if unnamed > 1 {
                    out.push_str(&unnamed.to_string());
                }
            }
        }
    }
    out
}

/// A concrete path for a pattern: parameters replaced by [`DUMMY`].
fn concrete_of(pattern: &[Segment]) -> String {
    pattern
        .iter()
        .map(|s| match s {
            Segment::Static(value) => format!("/{value}"),
            Segment::Param(_) => format!("/{DUMMY}"),
        })
        .collect()
}

/// Sends `OPTIONS` for every candidate and records the routes that answer.
async fn probe_all(
    client: &SockClient,
    candidates: BTreeSet<String>,
    probed: &mut BTreeSet<String>,
    inventory: &mut Inventory,
) {
    let permits = Arc::new(Semaphore::new(CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    for candidate in candidates {
        probed.insert(candidate.clone());
        let target = concrete(&candidate);
        if crate::probe::is_forbidden(&target) {
            inventory.skipped_danger.insert(candidate);
            continue;
        }
        inventory.requests += 1;
        let (client, permits) = (client.clone(), Arc::clone(&permits));
        tasks.spawn(async move {
            let _permit = permits.acquire_owned().await.ok()?;
            let response = client.request("OPTIONS", &target).await.ok()?;
            let allow = response.headers.get("allow")?.to_str().ok()?.to_owned();
            (response.status.as_u16() != 404).then_some((candidate, allow))
        });
    }
    while let Some(joined) = tasks.join_next().await {
        if let Ok(Some((route, allow))) = joined {
            inventory.routes.insert(route, allow);
        }
    }
}

/// Every path-like literal of `binary`: `/segment(/segment|/:param|/%s)*`.
/// Format verbs of client-side strings (`/sandbox/%s/exec`) become unnamed
/// parameters (`:`).
fn extract(binary: &[u8]) -> BTreeSet<String> {
    static PATH: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"/[a-z][a-z0-9_-]*(?:/(?:[a-z][a-z0-9_.-]*|:[a-z_]+|%[sdv]))*")
            .expect("valid regex")
    });
    PATH.find_iter(binary)
        .filter_map(|m| std::str::from_utf8(m.as_bytes()).ok())
        .map(|literal| {
            literal
                .split('/')
                .map(|s| if s.starts_with('%') { ":" } else { s })
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect()
}

/// The shape of a sequence of segments: `:x` segments are parameters.
fn shape(segments: &[&str]) -> Vec<Segment> {
    segments
        .iter()
        .map(|s| {
            if s.starts_with(':') {
                Segment::Param(None)
            } else {
                Segment::Static((*s).to_owned())
            }
        })
        .collect()
}

/// Shapes of every prefix of a pattern, the pattern included.
fn node_shapes(pattern: &str) -> Vec<Vec<Segment>> {
    let segments: Vec<&str> = pattern.split('/').skip(1).collect();
    (1..=segments.len())
        .map(|n| shape(&segments[..n]))
        .collect()
}

/// Candidates made of a known node of `literal` followed by the next
/// segments, the last one cut at every length (two characters at least).
fn glued_children(literal: &str, nodes: &BTreeSet<Vec<Segment>>) -> Vec<String> {
    let segments: Vec<&str> = literal.split('/').skip(1).collect();
    let mut out = Vec::new();
    for i in 1..segments.len() {
        if !nodes.contains(&shape(&segments[..i])) {
            continue;
        }
        for j in i..segments.len() {
            let base = format!("/{}", segments[..j].join("/"));
            let last = segments[j];
            if last.starts_with(':') {
                out.push(format!("{base}/{last}"));
                continue;
            }
            for end in 2..=last.len() {
                if last.is_char_boundary(end) {
                    out.push(format!("{base}/{}", &last[..end]));
                }
            }
        }
    }
    out
}

/// `/a/b/c` → `/a`, `/a/b`, `/a/b/c`.
fn prefixes(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for segment in path.split('/').skip(1) {
        current.push('/');
        current.push_str(segment);
        out.push(current.clone());
    }
    out
}

/// The suffixes of `path` that start at a segment in `roots` (not the first one).
fn suffixes(path: &str, roots: &BTreeSet<String>) -> Vec<String> {
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    (1..segments.len())
        .filter(|&i| roots.contains(segments[i]))
        .map(|i| format!("/{}", segments[i..].join("/")))
        .collect()
}

/// The pattern with every `:param` segment replaced by [`DUMMY`].
fn concrete(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|segment| {
            if segment.starts_with(':') {
                DUMMY
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The `sbx` binary found in `PATH`, symbolic links resolved.
///
/// # Errors
///
/// Fails if `sbx` is not in `PATH`.
pub(crate) fn find_sbx(name: &str) -> Result<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return candidate
            .canonicalize()
            .with_context(|| format!("{name} not found"));
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .and_then(|path| path.canonicalize().ok())
        .with_context(|| format!("{name} is not in PATH"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_are_extracted_with_their_parameters() {
        let found = extract(b"\x00/sandbox/:name/exec/attach\x00GET /daemon/health xyz/usr\x01");
        assert!(found.contains("/sandbox/:name/exec/attach"));
        assert!(found.contains("/daemon/health"));
    }

    #[test]
    fn candidates_cover_prefixes_and_glued_suffixes() {
        assert_eq!(prefixes("/a/b/c"), ["/a", "/a/b", "/a/b/c"]);
        let roots = BTreeSet::from(["sandbox".to_owned()]);
        assert_eq!(
            suffixes("/daemon/info/sandbox/:name/start", &roots),
            ["/sandbox/:name/start"]
        );
        assert_eq!(
            concrete("/sandbox/:name/exec/:exec_id"),
            "/sandbox/zz-audit/exec/zz-audit"
        );
    }

    #[test]
    fn glued_literals_are_cut_under_known_nodes() {
        let nodes: BTreeSet<Vec<Segment>> = node_shapes("/sandbox/:name").into_iter().collect();
        let found = glued_children("/sandbox/:name/stopx", &nodes);
        for expected in [
            "/sandbox/:name/st",
            "/sandbox/:name/stop",
            "/sandbox/:name/stopx",
        ] {
            assert!(found.contains(&expected.to_owned()), "{expected} missing");
        }
        assert!(!found.contains(&"/sandbox/:name/s".to_owned()));
        assert!(
            glued_children("/sandbox/:name/debug/vmx", &nodes)
                .contains(&"/sandbox/:name/debug/vm".to_owned())
        );
        assert!(extract(b"/sandbox/%s/swap-container\x00").contains("/sandbox/:/swap-container"));
    }

    #[test]
    fn patterns_prefer_short_parameter_names() {
        let named = vec![
            Segment::Static("sandbox".into()),
            Segment::Param(Some("name".into())),
        ];
        let glued = vec![
            Segment::Static("sandbox".into()),
            Segment::Param(Some("namesetting".into())),
        ];
        let unnamed = vec![Segment::Static("sandbox".into()), Segment::Param(None)];
        assert!(names_score(&named) > names_score(&glued));
        assert!(names_score(&glued) > names_score(&unnamed));
        assert_eq!(render(&unnamed), "/sandbox/:param");
        assert_eq!(concrete_of(&named), "/sandbox/zz-audit");
    }
}
