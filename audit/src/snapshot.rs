//! Audit snapshots: everything measured on one `sbx` version.
//!
//! [`audit`] replays every suite against the live daemon and writes a snapshot
//! directory, named after the `sbx` version:
//!
//! ```text
//! <snapshots>/<version>/
//!   manifest.json         sbx version, date, suite outcomes, contract size
//!   routes.json           router inventory (binary scan + OPTIONS)
//!   router-log.json       router patterns the daemon matched during the run
//!   events.ndjson         GET /events stream recorded during the run
//!   captures/<suite>.jsonl  spy capture of each suite
//!   results/<suite>.json    outcome of each scenario
//!   api/openapi.json      OpenAPI 3.1 contract
//!   api/API.md            Markdown reference
//! ```
//!
//! Everything is scrubbed when it is recorded, and the snapshot is only moved
//! into place once [`privacy::check`] finds nothing. The
//! daemon is left as it was found: settings changed during the run are restored,
//! and sandboxes and images created by the run are removed (see
//! [`DaemonState`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::assets::{self, WorkDir};
use crate::client::SockClient;
use crate::privacy::{self, Scrubber};
use crate::scenarios::{self, ScenarioResult, Verdict};
use crate::spec::{self, SpecSummary};

/// Prefixes of the sandbox names the suites create.
const AUDIT_SANDBOXES: &[&str] = &["audit-", "docker-sandbox-audit-", "sbx-kit-compile-"];

/// How many times the guard lists and removes new images: removing the last
/// tag of an image can reveal it under its digest reference.
const IMAGE_REMOVAL_ROUNDS: usize = 3;

/// The files of a snapshot directory.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    /// Root of the snapshot.
    pub dir: PathBuf,
}

impl Snapshot {
    /// The snapshot at `dir`.
    #[must_use]
    pub(crate) fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `manifest.json`.
    #[must_use]
    pub(crate) fn manifest(&self) -> PathBuf {
        self.dir.join("manifest.json")
    }

    /// `routes.json`.
    #[must_use]
    pub(crate) fn routes(&self) -> PathBuf {
        self.dir.join("routes.json")
    }

    /// `router-log.json`.
    #[must_use]
    pub(crate) fn router_log(&self) -> PathBuf {
        self.dir.join("router-log.json")
    }

    /// `events.ndjson`.
    #[must_use]
    pub(crate) fn events(&self) -> PathBuf {
        self.dir.join("events.ndjson")
    }

    /// Capture of the suite `name`.
    #[must_use]
    pub(crate) fn capture(&self, name: &str) -> PathBuf {
        self.dir.join("captures").join(format!("{name}.jsonl"))
    }

    /// Results of the suite `name`.
    #[must_use]
    pub(crate) fn results(&self, name: &str) -> PathBuf {
        self.dir.join("results").join(format!("{name}.json"))
    }

    /// `api/`, where the contract is generated.
    #[must_use]
    pub(crate) fn api(&self) -> PathBuf {
        self.dir.join("api")
    }

    /// Every capture of the snapshot, sorted by name.
    ///
    /// # Errors
    ///
    /// Fails if the `captures` directory cannot be listed.
    pub(crate) fn captures(&self) -> Result<Vec<PathBuf>> {
        let dir = self.dir.join("captures");
        let mut captures: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("listing {}", dir.display()))?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        captures.sort();
        Ok(captures)
    }

    /// Generates `api/openapi.json` and `api/API.md` from the snapshot.
    ///
    /// # Errors
    ///
    /// Fails if an input cannot be read or an output cannot be written.
    pub(crate) fn generate_spec(&self) -> Result<SpecSummary> {
        let captures = self.captures()?;
        let (routes, router_log, events) = (self.routes(), self.router_log(), self.events());
        spec::run(&spec::SpecOpts {
            captures: &captures,
            routes: Some(&routes),
            router_log: Some(&router_log),
            events: events.exists().then_some(events.as_path()),
            notes: Some(assets::NOTES),
            out: &self.api(),
        })
    }
}

/// `manifest.json` of a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Manifest {
    /// `sbx` version, as reported by the daemon.
    pub sbx_version: String,
    /// API version of the daemon.
    pub api_version: String,
    /// Day of the audit (UTC), `YYYY-MM-DD`.
    pub audited_on: String,
    /// Version of `audit` that produced the snapshot.
    pub tool_version: String,
    /// Outcome of each suite.
    pub suites: Vec<SuiteSummary>,
    /// Routes in the router inventory.
    pub routes: usize,
    /// Events recorded on `GET /events`.
    pub events: usize,
    /// Size of the generated contract.
    pub contract: SpecSummary,
}

/// Outcome of one suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SuiteSummary {
    /// Suite name.
    pub name: String,
    /// Scenarios run.
    pub scenarios: usize,
    /// Scenarios that failed although expected to succeed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
    /// Scenarios that succeeded although expected to fail.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unexpected_success: Vec<String>,
}

impl SuiteSummary {
    /// Summarizes the results of the suite `name`.
    fn new(name: &str, results: &[ScenarioResult]) -> Self {
        let with = |verdict| {
            results
                .iter()
                .filter(|r| r.verdict == verdict)
                .map(|r| r.name.clone())
                .collect()
        };
        Self {
            name: name.to_owned(),
            scenarios: results.len(),
            failed: with(Verdict::Failed),
            unexpected_success: with(Verdict::UnexpectedSuccess),
        }
    }
}

/// Options of [`audit`].
pub(crate) struct AuditOpts<'a> {
    /// `sbx` executable.
    pub sbx: &'a str,
    /// Socket of the real daemon.
    pub socket: &'a Path,
    /// Directory holding one snapshot per `sbx` version.
    pub snapshots: &'a Path,
    /// Where the snapshot is assembled before being moved into place.
    pub staging: &'a Path,
    /// Suites to replay, in order.
    pub suites: &'a [String],
    /// Replace an existing snapshot of the same version.
    pub force: bool,
    /// Skip the router inventory (for a quick partial run).
    pub skip_routes: bool,
    /// Use swap mode for the spy.
    pub swap: bool,
    /// Maximum duration of one scenario.
    pub timeout: Duration,
    /// Scrubber applied to everything recorded.
    pub scrubber: Arc<Scrubber>,
}

/// Runs the whole audit and returns the directory of the new snapshot.
///
/// The daemon state is read before the replay and restored after it, even when
/// a suite fails or the audit is interrupted with Ctrl-C.
///
/// # Errors
///
/// Fails if the daemon cannot be reached, if a snapshot of the same version
/// exists without `force`, if a suite cannot run, if the audit is interrupted,
/// or if personal data is left in the snapshot (which is then kept in the
/// staging directory for review).
pub(crate) async fn audit(o: AuditOpts<'_>) -> Result<PathBuf> {
    let daemon = SockClient::new(o.socket);
    let health: Value = serde_json::from_slice(
        &daemon
            .request("GET", "/daemon/health")
            .await
            .context("the daemon does not answer")?
            .body,
    )
    .context("parsing /daemon/health")?;
    let version = health["version"].as_str().unwrap_or("unknown").to_owned();
    let api_version = health["api_version"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    let destination = o.snapshots.join(&version);
    if destination.exists() && !o.force {
        bail!(
            "{} already exists: pass --force to replace it",
            destination.display()
        );
    }
    eprintln!("[audit] sbx {version} (API {api_version})");

    let staging = Snapshot::new(o.staging.join(&version));
    if staging.dir.exists() {
        std::fs::remove_dir_all(&staging.dir)?;
    }
    for sub in ["captures", "results", "api"] {
        std::fs::create_dir_all(staging.dir.join(sub))?;
    }

    let baseline = DaemonState::read(&daemon, o.sbx).await?;
    // On Ctrl-C the replay is dropped — which stops the spy, closes the held
    // sessions and kills the running command — and the daemon is still
    // restored below.
    let replayed = tokio::select! {
        replayed = replay(&o, &staging) => replayed,
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("interrupted")),
    };
    let after = DaemonState::read(&daemon, o.sbx).await?;
    baseline.restore(&after, &daemon).await;
    let suites = replayed?;

    let contract = staging.generate_spec()?;
    let manifest = Manifest {
        sbx_version: version,
        api_version,
        audited_on: utc_date(),
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        suites,
        routes: read_json(&staging.routes())
            .ok()
            .and_then(|v| v["routes"].as_object().map(serde_json::Map::len))
            .unwrap_or(0),
        events: count_lines(&staging.events()),
        contract,
    };
    write_json(&staging.manifest(), &manifest, &o.scrubber)?;

    let findings = privacy::check(std::slice::from_ref(&staging.dir), &o.scrubber)?;
    if !findings.is_empty() {
        for finding in &findings {
            eprintln!(
                "!! {}:{}: {}",
                finding.file.display(),
                finding.line,
                finding.what
            );
        }
        bail!(
            "personal data left in the snapshot ({} findings): it stays in {} for review",
            findings.len(),
            staging.dir.display()
        );
    }

    if destination.exists() {
        std::fs::remove_dir_all(&destination)?;
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    move_dir(&staging.dir, &destination)?;
    for suite in &manifest.suites {
        for (label, names) in [
            ("failed", &suite.failed),
            ("unexpectedly succeeded", &suite.unexpected_success),
        ] {
            if !names.is_empty() {
                eprintln!("!! {}: {label}: {}", suite.name, names.join(", "));
            }
        }
    }
    eprintln!(
        "[audit] snapshot written to {}: {} operations ({} observed)",
        destination.display(),
        manifest.contract.operations,
        manifest.contract.observed
    );
    Ok(destination)
}

/// Records the inventory, the event stream, the suites and the router log into
/// `staging`. Returns the outcome of each suite.
async fn replay(o: &AuditOpts<'_>, staging: &Snapshot) -> Result<Vec<SuiteSummary>> {
    let work = WorkDir::prepare(&WorkDir::default_root())?;
    // Removed on every exit, the interrupted ones included.
    let _cleanup = RemoveOnDrop(work.root.clone());
    let daemon_log = o.socket.with_file_name("daemon.log");

    if o.skip_routes {
        eprintln!("[audit] router inventory skipped");
    } else {
        let binary = crate::routes::find_sbx(o.sbx)?;
        eprintln!("[audit] router inventory of {}", binary.display());
        let inventory = crate::routes::inventory(o.socket, &binary).await?;
        eprintln!(
            "[audit] {} routes ({} candidates, {} requests)",
            inventory.routes.len(),
            inventory.candidates,
            inventory.requests
        );
        write_json(&staging.routes(), &inventory, &o.scrubber)?;
    }

    // Marked after the inventory: the router does not log patterns for OPTIONS.
    let log_mark = LogMark::now(&daemon_log);
    let events = EventRecorder::start(o.socket, &staging.events(), Arc::clone(&o.scrubber))?;
    let mut summaries = Vec::new();
    for name in o.suites {
        let suite = assets::suite(name)?;
        eprintln!("\n[audit] suite {name}: {}", suite.description);
        let results = scenarios::run(scenarios::RunOpts {
            sbx: o.sbx,
            upstream: o.socket,
            work: &work,
            capture: &staging.capture(name),
            suite: &suite,
            only: &[],
            swap: o.swap,
            redact: true,
            scrubber: Arc::clone(&o.scrubber),
            timeout: o.timeout,
        })
        .await?;
        write_json(&staging.results(name), &results, &o.scrubber)?;
        summaries.push(SuiteSummary::new(name, &results));
    }
    drop(events);

    let routes = log_mark.routes_since()?;
    if routes.is_empty() {
        eprintln!(
            "!! no router pattern found in {}: the exact parameter names are missing",
            daemon_log.display()
        );
    }
    let routes: BTreeMap<String, String> = routes
        .into_iter()
        .map(|(route, methods)| (route, methods.into_iter().collect::<Vec<_>>().join(", ")))
        .collect();
    write_json(
        &staging.router_log(),
        &json!({ "routes": routes }),
        &o.scrubber,
    )?;
    Ok(summaries)
}

/// A position in the daemon log that survives log rotation.
///
/// The daemon rotates `daemon.log` into `daemon-<time>.log.gz` next to it. When
/// archives appear after the mark, the first one holds the rest of the file the
/// mark points into, and the later ones (then `daemon.log`) are read whole.
struct LogMark {
    /// The live log file.
    log: PathBuf,
    /// Its size when the mark was taken.
    offset: usize,
    /// The archives that existed when the mark was taken.
    archives: BTreeSet<PathBuf>,
}

impl LogMark {
    /// Marks the current end of `log`.
    fn now(log: &Path) -> Self {
        let offset =
            std::fs::metadata(log).map_or(0, |m| usize::try_from(m.len()).unwrap_or(usize::MAX));
        Self {
            log: log.to_path_buf(),
            offset,
            archives: rotated_logs(log),
        }
    }

    /// The router patterns logged since the mark.
    fn routes_since(&self) -> Result<BTreeMap<String, BTreeSet<String>>> {
        let mut routes = BTreeMap::new();
        let mut offset = self.offset;
        for archive in rotated_logs(&self.log).difference(&self.archives) {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(
                &mut flate2::read::GzDecoder::new(
                    std::fs::File::open(archive).with_context(|| archive.display().to_string())?,
                ),
                &mut bytes,
            )
            .with_context(|| archive.display().to_string())?;
            let tail = bytes.get(offset..).unwrap_or_default();
            spec::router_patterns(&String::from_utf8_lossy(tail), &mut routes);
            offset = 0;
        }
        if let Ok(bytes) = std::fs::read(&self.log) {
            // A log shorter than the mark was truncated: read it whole.
            let tail = bytes.get(offset..).unwrap_or(&bytes);
            spec::router_patterns(&String::from_utf8_lossy(tail), &mut routes);
        }
        Ok(routes)
    }
}

/// The rotated archives of `log` (`daemon-<time>.log.gz`), oldest first.
fn rotated_logs(log: &Path) -> BTreeSet<PathBuf> {
    let Some(dir) = log.parent() else {
        return BTreeSet::new();
    };
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("daemon-") && n.ends_with(".log.gz"))
        })
        .collect()
}

/// A directory removed when the value is dropped.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Records `GET /events` into a file for as long as it lives.
struct EventRecorder(tokio::task::JoinHandle<()>);

impl EventRecorder {
    /// Starts recording the events of the daemon on `socket` into `file`.
    fn start(socket: &Path, file: &Path, scrubber: Arc<Scrubber>) -> Result<Self> {
        use std::io::Write as _;
        let mut out = std::io::LineWriter::new(
            std::fs::File::create(file).with_context(|| file.display().to_string())?,
        );
        let client = SockClient::new(socket);
        Ok(Self(tokio::spawn(async move {
            let _ = client
                .stream_lines("/events", |line| {
                    let _ = writeln!(out, "{}", scrubber.line(line));
                })
                .await;
        })))
    }
}

impl Drop for EventRecorder {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What the audit must leave as it found it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct DaemonState {
    /// Settings: key → (source, value).
    settings: BTreeMap<String, (String, Value)>,
    /// Sandboxes: name → id.
    sandboxes: BTreeMap<String, String>,
    /// Image references.
    images: BTreeSet<String>,
    /// Identifiers of the network rules.
    rules: BTreeSet<String>,
    /// Log levels.
    log_levels: Value,
    /// Read-only CLI listings that have no socket route: secrets and MCP
    /// servers.
    listings: BTreeMap<String, Listing>,
}

/// What a CLI listing tells, without its text: `sbx secret ls` shows the
/// beginning and the end of the value of each custom secret, which must not be
/// kept, not even in a scratch file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Listing {
    /// Entries listed, the header aside.
    entries: usize,
    /// Fingerprint of the whole listing, to tell that it changed. It comes from
    /// the hasher of the standard library: a record is checked by the build
    /// that made it.
    fingerprint: String,
}

impl Listing {
    /// The listing `text`, reduced to its entry count and fingerprint.
    fn of(text: &str) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(text, &mut hasher);
        Self {
            entries: text
                .lines()
                .skip(1)
                .filter(|line| !line.trim().is_empty())
                .count(),
            fingerprint: format!("{:016x}", std::hash::Hasher::finish(&hasher)),
        }
    }
}

impl DaemonState {
    /// Reads the current state through the socket and the CLI.
    ///
    /// # Errors
    ///
    /// Fails if the socket cannot be reached.
    pub(crate) async fn read(daemon: &SockClient, sbx: &str) -> Result<Self> {
        let get = |path: &'static str| async move {
            let response = daemon.request("GET", path).await?;
            anyhow::Ok(serde_json::from_slice::<Value>(&response.body).unwrap_or(Value::Null))
        };
        let settings = get("/daemon/settings").await?["settings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                Some((
                    s["key"].as_str()?.to_owned(),
                    (
                        s["source"].as_str().unwrap_or_default().to_owned(),
                        s["value"].clone(),
                    ),
                ))
            })
            .collect();
        let sandboxes = get("/sandbox")
            .await?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                Some((
                    s["name"].as_str()?.to_owned(),
                    s["id"].as_str().unwrap_or_default().to_owned(),
                ))
            })
            .collect();
        let images = list_images(daemon).await?;
        let rules = get("/policy/network/rules").await?["rules"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["id"].as_str().map(str::to_owned))
            .collect();
        let log_levels = get("/daemon/loglevel").await?;
        // Templates are images of the daemon, already compared through
        // `images`; their CLI listing would only add relative dates.
        let mut listings = BTreeMap::new();
        for (name, args) in [
            ("secrets", ["secret", "ls"]),
            ("mcp servers", ["mcp", "ls"]),
        ] {
            let output = tokio::process::Command::new(sbx)
                .arg("--no-input")
                .args(args)
                .output()
                .await;
            if let Ok(output) = output {
                let text = String::from_utf8_lossy(&output.stdout);
                listings.insert(name.to_owned(), Listing::of(text.trim()));
            }
        }
        Ok(Self {
            settings,
            sandboxes,
            images,
            rules,
            log_levels,
            listings,
        })
    }

    /// One line per part of the state: sandboxes, images, network rules,
    /// overridden settings, and the secrets and MCP servers `sbx` lists.
    pub(crate) fn summary(&self) -> Vec<String> {
        let names = |items: Vec<&String>| {
            if items.is_empty() {
                "none".to_owned()
            } else {
                items
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        let overridden: Vec<&String> = self
            .settings
            .iter()
            .filter(|(_, (source, _))| source != "default")
            .map(|(key, _)| key)
            .collect();
        let mut lines = vec![
            format!("sandboxes: {}", names(self.sandboxes.keys().collect())),
            format!(
                "images: {} ({})",
                self.images.len(),
                names(self.images.iter().collect())
            ),
            format!("network rules: {}", self.rules.len()),
            format!("settings overridden: {}", names(overridden)),
        ];
        for (name, listing) in &self.listings {
            lines.push(format!("{name} listed by sbx: {}", listing.entries));
        }
        lines
    }

    /// What differs in `now`, one line each; empty when the states match.
    pub(crate) fn differences(&self, now: &Self) -> Vec<String> {
        let mut lines = Vec::new();
        for (key, before) in &self.settings {
            if now.settings.get(key) != Some(before) {
                lines.push(format!("setting {key} changed"));
            }
        }
        for added in now
            .sandboxes
            .keys()
            .filter(|n| !self.sandboxes.contains_key(*n))
        {
            lines.push(format!("sandbox {added} appeared"));
        }
        for gone in self
            .sandboxes
            .keys()
            .filter(|n| !now.sandboxes.contains_key(*n))
        {
            lines.push(format!("sandbox {gone} disappeared"));
        }
        for added in now.images.difference(&self.images) {
            lines.push(format!("image {added} appeared"));
        }
        for gone in self.images.difference(&now.images) {
            lines.push(format!("image {gone} disappeared"));
        }
        if self.rules != now.rules {
            lines.push(format!(
                "network rules changed: {} → {}",
                self.rules.len(),
                now.rules.len()
            ));
        }
        if self.log_levels != now.log_levels {
            lines.push(format!(
                "log levels changed: {} → {}",
                self.log_levels, now.log_levels
            ));
        }
        for (name, before) in &self.listings {
            if now.listings.get(name).is_some_and(|after| after != before) {
                lines.push(format!("the {name} listed by sbx changed"));
            }
        }
        lines
    }

    /// Brings the daemon back from `now` to this state where it can, and
    /// reports what it cannot restore. Nothing is printed when the states match.
    pub(crate) async fn restore(&self, now: &Self, daemon: &SockClient) {
        let mut report = Vec::new();
        for (key, (source, value)) in &self.settings {
            if now.settings.get(key) == Some(&(source.clone(), value.clone())) {
                continue;
            }
            let path = format!("/daemon/settings/{key}");
            let restored = if source == "default" {
                send(daemon, "DELETE", &path, None).await
            } else {
                send(daemon, "POST", &path, Some(json!({ "value": value }))).await
            };
            report.push(match restored {
                Ok(()) => format!("setting {key} restored"),
                Err(e) => format!("!! setting {key} could not be restored: {e:#}"),
            });
        }
        for (name, id) in &now.sandboxes {
            if self.sandboxes.contains_key(name) {
                continue;
            }
            if AUDIT_SANDBOXES.iter().any(|p| name.starts_with(p)) {
                let path = format!("/sandbox/{name}?expected_id={id}&force=true");
                report.push(match send(daemon, "DELETE", &path, None).await {
                    Ok(()) => format!("leftover sandbox {name} removed"),
                    Err(e) => format!("!! leftover sandbox {name} could not be removed: {e:#}"),
                });
            } else {
                report.push(format!(
                    "!! sandbox {name} appeared during the audit, left untouched"
                ));
            }
        }
        // Removing the last tag of an image can reveal it under its digest
        // reference: the images are listed again until no new one is left.
        let mut current = now.images.clone();
        let mut failed = BTreeSet::new();
        for _ in 0..IMAGE_REMOVAL_ROUNDS {
            // Derived tags first, so that their parent image can go afterwards.
            let mut created: Vec<String> = current
                .difference(&self.images)
                .filter(|reference| !failed.contains(*reference))
                .cloned()
                .collect();
            if created.is_empty() {
                break;
            }
            created.sort_by_key(|reference| std::cmp::Reverse(reference.len()));
            for reference in created {
                let path = format!("/docker/images/remove?name={}", encode(&reference));
                if let Err(e) = send(daemon, "DELETE", &path, None).await {
                    report.push(format!("!! image {reference} could not be removed: {e:#}"));
                    failed.insert(reference);
                } else {
                    report.push(format!("image {reference} removed"));
                }
            }
            match list_images(daemon).await {
                Ok(listed) => current = listed,
                Err(e) => {
                    report.push(format!("!! images could not be listed again: {e:#}"));
                    break;
                }
            }
        }
        for gone in self.images.difference(&now.images) {
            report.push(format!("!! image {gone} disappeared during the audit"));
        }
        if self.rules != now.rules {
            report.push("!! the network rules changed during the audit".to_owned());
        }
        if self.log_levels != now.log_levels {
            report.push(format!(
                "!! log levels changed: {} → {}",
                self.log_levels, now.log_levels
            ));
        }
        for (name, before) in &self.listings {
            if now.listings.get(name).is_some_and(|after| after != before) {
                report.push(format!(
                    "!! the {name} listed by sbx changed during the audit"
                ));
            }
        }
        if report.is_empty() {
            eprintln!("[audit] daemon state unchanged");
        }
        for line in report {
            eprintln!("[audit] {line}");
        }
    }
}

/// References of every image held by the daemon.
async fn list_images(daemon: &SockClient) -> Result<BTreeSet<String>> {
    let response = daemon.request("GET", "/docker/images?all=true").await?;
    let images: Value = serde_json::from_slice(&response.body).context("parsing the image list")?;
    Ok(images
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(image_reference)
        .collect())
}

/// Reference of an image listed by `GET /docker/images`.
fn image_reference(image: &Value) -> Option<String> {
    let repository = image["repository"].as_str()?;
    Some(match image["tag"].as_str().filter(|t| !t.is_empty()) {
        Some(tag) => format!("{repository}:{tag}"),
        None => match image["digest"].as_str().filter(|d| !d.is_empty()) {
            Some(digest) => format!("{repository}@{digest}"),
            None => repository.to_owned(),
        },
    })
}

/// Sends a request and fails on an error status.
async fn send(daemon: &SockClient, method: &str, path: &str, body: Option<Value>) -> Result<()> {
    let body = body.map(|b| bytes::Bytes::from(b.to_string()));
    let response = daemon
        .send(SockClient::build(method, path, &[], body)?)
        .await?;
    if response.status.as_u16() >= 400 {
        bail!(
            "HTTP {}: {}",
            response.status,
            String::from_utf8_lossy(&response.body).trim()
        );
    }
    Ok(())
}

/// Percent-encodes a query value.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Writes `value` as pretty JSON to `path`, scrubbed, with a final newline.
fn write_json(path: &Path, value: &impl Serialize, scrubber: &Scrubber) -> Result<()> {
    let mut value = serde_json::to_value(value)?;
    scrubber.value(&mut value);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    std::fs::write(path, bytes).with_context(|| path.display().to_string())
}

/// Reads a JSON file.
fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&std::fs::read(path).with_context(|| path.display().to_string())?)
        .with_context(|| path.display().to_string())
}

/// Number of non-empty lines of a file, 0 when it cannot be read.
fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |text| {
        text.lines().filter(|l| !l.trim().is_empty()).count()
    })
}

/// Moves a directory, copying it when a rename is not possible.
fn move_dir(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_dir(from, to)?;
    std::fs::remove_dir_all(from)?;
    Ok(())
}

/// Copies a directory recursively.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Today's date in UTC, `YYYY-MM-DD`.
fn utc_date() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (year, month, day) = civil_date(i64::try_from(seconds / 86_400).unwrap_or(0));
    format!("{year:04}-{month:02}-{day:02}")
}

/// The proleptic Gregorian date of a day count since 1970-01-01.
fn civil_date(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listings_keep_no_secret_text() {
        let text = "SCOPE      TYPE      NAME        SECRET\n\
                    (global)   service   anthropic   (oauth configured)\n\
                    audit-a    custom    AUDIT_KEY   dummy-******...******cret";
        let listing = Listing::of(text);
        assert_eq!(listing.entries, 2);
        let saved = serde_json::to_string(&listing).unwrap();
        assert!(
            !saved.contains("dummy") && !saved.contains("anthropic"),
            "{saved}"
        );
        assert_ne!(
            listing,
            Listing::of("SCOPE      TYPE      NAME        SECRET")
        );
    }

    #[test]
    fn dates_are_computed_in_utc() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(20_416), (2025, 11, 24));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
    }

    #[test]
    fn image_references_prefer_tags() {
        assert_eq!(
            image_reference(&json!({"repository": "docker.io/sbx/shell", "tag": "latest"}))
                .unwrap(),
            "docker.io/sbx/shell:latest"
        );
        assert_eq!(
            image_reference(&json!({"repository": "docker.io/sbx/shell", "digest": "sha256:1"}))
                .unwrap(),
            "docker.io/sbx/shell@sha256:1"
        );
        assert_eq!(encode("a/b:c"), "a%2Fb%3Ac");
    }
}
