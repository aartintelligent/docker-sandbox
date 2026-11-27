//! `audit`: maps what can be driven through the `sandboxd` socket of
//! Docker Sandboxes (`sbx`) without going through the CLI, and keeps that map
//! up to date across `sbx` versions.
//!
//! The upgrade workflow (see `audit/README.md`):
//!
//! 1. `audit`: replays every suite against the live daemon and writes a
//!    snapshot of the new version (captures, router inventory, events,
//!    OpenAPI contract), free of personal data;
//! 2. `compare`: lists what changed since the previous snapshot;
//! 3. `fixtures`: refreshes the test fixtures of `docker-sandbox`, then the crates
//!    are adapted until their tests pass.
//!
//! | Command | Purpose |
//! |---|---|
//! | `audit` | Full audit of the live daemon into a versioned snapshot |
//! | `compare` | API changes between two snapshots |
//! | `spec` | Regenerates the contract of a snapshot |
//! | `fixtures` | Exports a snapshot as `docker-sandbox` test fixtures |
//! | `privacy-check`, `scrub` | Looks for personal data in audit files; scrubs a file |
//! | `scenarios` | Replays one suite behind the spy |
//! | `requests` | Requests of a capture, scenario by scenario |
//! | `state` | Records the state of the daemon, then checks it is unchanged |
//! | `routes`, `router-log` | Router inventory (binary scan + `OPTIONS`); patterns matched by the daemon |
//! | `probe` | Passive probing of known and candidate routes (`GET`/`OPTIONS` only) |
//! | `perms` | Socket permissions and daemon identity |
//! | `spy` | Interactive capture of the CLI traffic |
//! | `summarize` | Route catalog and scenario matrix of a capture |
//! | `kit-compile` | Kit packages replayable through the socket alone |
//! | `kit-forge` | Experiment: the package of a v3 mixin written without the CLI |
//! | `raw`, `replay` | Arbitrary or captured requests sent straight to the socket |
//!
//! Run it as the user owning the daemon, with a pinned `sbx` version.
//! Destructive daemon routes (`/daemon/reset`, `/daemon/shutdown`) are never
//! called by the automated commands.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line tool reports on standard output and standard error"
)]

mod assets;
mod client;
mod compare;
mod fixtures;
mod kitbase;
mod kitc;
mod kitforge;
mod mcp_echo;
mod perms;
mod privacy;
mod probe;
mod pty;
mod raw;
mod requests;
mod routes;
mod scenarios;
mod snapshot;
mod spec;
mod spy;
mod state;
mod summarize;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::privacy::Scrubber;

/// Snapshots of the audited `sbx` versions, next to the sources of the tool.
const SNAPSHOTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/snapshots");
/// Scratch output, in the target directory of the workspace.
const OUTPUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/audit");
/// Root of the workspace.
const WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
/// The crates the workspace publishes, whose files the privacy check covers.
const PUBLISHED_CRATES: &[&str] = &["docker-sandbox", "docker-sandbox-kit"];
/// Test fixtures of the library crate.
const LIBRARY_FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docker-sandbox/tests/fixtures"
);

/// Command line of `audit`.
#[derive(Parser)]
#[command(
    name = "audit",
    version,
    about = "Audit tool for the sandboxd socket of Docker Sandboxes (sbx)"
)]
struct Cli {
    /// Daemon socket. Defaults to `$DOCKER_SANDBOXES_API`, then `sandboxd.sock` in the sbx
    /// state directory (`$XDG_STATE_HOME/sandboxes/sandboxes/sandboxd`, where
    /// `$XDG_STATE_HOME` defaults to `~/.local/state`).
    #[arg(long, global = true, env = "DOCKER_SANDBOX_AUDIT_SOCK")]
    sock: Option<PathBuf>,
    /// Additional private value to scrub from everything recorded, such as a Docker
    /// account name (repeatable, comma-separated; also read from `DOCKER_SANDBOX_AUDIT_PRIVATE`)
    #[arg(long, global = true, value_delimiter = ',')]
    private: Vec<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

/// The subcommands.
#[derive(Subcommand)]
enum Cmd {
    /// Full audit of the live daemon into a versioned snapshot
    Audit(AuditArgs),
    /// API changes between two snapshots, as Markdown
    Compare(CompareArgs),
    /// Regenerate the OpenAPI contract and Markdown reference of a snapshot
    Spec(SpecArgs),
    /// Export a snapshot as docker-sandbox test fixtures (one file per method, route and status)
    Fixtures(FixturesArgs),
    /// Look for personal data in audit files; fails when some is found
    PrivacyCheck(PrivacyCheckArgs),
    /// Copy a file (JSON lines or text) with its personal data scrubbed
    Scrub(ScrubArgs),
    /// Replay one suite behind the spy, then restore the daemon as `audit` does
    Scenarios(ScenariosArgs),
    /// Router inventory: route literals of the sbx binary confirmed with OPTIONS
    Routes(RoutesArgs),
    /// Router patterns the daemon matched, extracted from its log
    RouterLog(RouterLogArgs),
    /// Probe known and candidate routes with GET and OPTIONS only
    Probe(ProbeArgs),
    /// Socket permissions and daemon identity (`SO_PEERCRED`)
    Perms(PermsArgs),
    /// Interactive spy: then point `DOCKER_SANDBOXES_API` at the --listen socket and use sbx
    Spy(SpyArgs),
    /// Route catalog and Markdown matrix of a capture
    Summarize(SummarizeArgs),
    /// Requests of a capture, scenario by scenario, with their bodies
    Requests(RequestsArgs),
    /// Record the state of the daemon (--save), or check it against the record (--check)
    State(StateArgs),
    /// Compile local kits into a package replayable through the socket alone
    KitCompile(KitCompileArgs),
    /// Experiment: write the package of a v3 mixin without the sbx CLI
    KitForge(KitForgeArgs),
    /// Replay a captured request straight to the socket
    Replay(ReplayArgs),
    /// Send an arbitrary request (--upgrade for exec/attach streams)
    Raw(RawArgs),
    /// Minimal MCP server on standard input and output, used by the coverage suite
    #[command(hide = true)]
    McpEcho,
}

impl Cmd {
    /// Runs the subcommand against the daemon socket `sock`.
    async fn run(self, sock: &Path, scrubber: Arc<Scrubber>) -> Result<()> {
        match self {
            Self::Audit(args) => args.run(sock, scrubber).await,
            Self::Compare(args) => args.run(),
            Self::Spec(args) => args.run(),
            Self::Fixtures(args) => args.run(&scrubber),
            Self::PrivacyCheck(args) => args.run(&scrubber),
            Self::Scrub(args) => args.run(&scrubber),
            Self::Scenarios(args) => args.run(sock, scrubber).await,
            Self::Routes(args) => args.run(sock, &scrubber).await,
            Self::RouterLog(args) => args.run(sock, &scrubber),
            Self::Probe(args) => args.run(sock).await,
            Self::Perms(args) => args.run(sock).await,
            Self::Spy(args) => args.run(sock, scrubber).await,
            Self::Summarize(args) => args.run(),
            Self::Requests(args) => requests::print(&args.capture, &args.only, args.reads),
            Self::State(args) => {
                state::run(sock, &args.sbx, &args.file, args.save, args.check).await
            }
            Self::KitCompile(args) => args.run(sock).await,
            Self::KitForge(args) => args.run(sock).await,
            Self::Replay(args) => {
                raw::replay(sock, &args.capture, args.conn, args.seq, args.dry_run).await
            }
            Self::Raw(args) => args.run(sock).await,
            Self::McpEcho => mcp_echo::run(),
        }
    }
}

/// Arguments of `audit`.
#[derive(Args)]
struct AuditArgs {
    /// Suites to replay, in order (default: every embedded suite)
    #[arg(long, value_delimiter = ',')]
    suites: Vec<String>,
    /// Directory holding one snapshot per sbx version
    #[arg(long, default_value = SNAPSHOTS)]
    snapshots: PathBuf,
    /// Replace an existing snapshot of the same version
    #[arg(long)]
    force: bool,
    /// Skip the router inventory (quick partial run)
    #[arg(long)]
    skip_routes: bool,
    /// Temporarily replace the real socket with the spy
    #[arg(long)]
    swap: bool,
    /// `sbx` executable
    #[arg(long, default_value = "sbx")]
    sbx: String,
    /// Timeout of each scenario, in seconds
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,
}

impl AuditArgs {
    async fn run(self, sock: &Path, scrubber: Arc<Scrubber>) -> Result<()> {
        let staging = Path::new(OUTPUT).join("runs");
        let suites = if self.suites.is_empty() {
            assets::SUITES
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect()
        } else {
            self.suites.clone()
        };
        let audit = snapshot::audit(snapshot::AuditOpts {
            sbx: &self.sbx,
            socket: sock,
            snapshots: &self.snapshots,
            staging: &staging,
            suites: &suites,
            force: self.force,
            skip_routes: self.skip_routes,
            swap: self.swap,
            timeout: Duration::from_secs(self.timeout_secs),
            scrubber,
        });
        // The audit handles Ctrl-C itself, so that the daemon state is
        // restored even when it is interrupted.
        let dir = audit.await?;
        println!("{}", dir.display());
        Ok(())
    }
}

/// Arguments of `compare`.
#[derive(Args)]
struct CompareArgs {
    /// Snapshot of the previous version
    old: PathBuf,
    /// Snapshot of the new version
    new: PathBuf,
    /// Write the report to this file instead of standard output
    #[arg(long)]
    out: Option<PathBuf>,
}

impl CompareArgs {
    fn run(self) -> Result<()> {
        let report = compare::run(&self.old, &self.new)?;
        if let Some(path) = self.out {
            write_text(&path, &report)
        } else {
            print!("{report}");
            Ok(())
        }
    }
}

/// Arguments of `spec`.
#[derive(Args)]
struct SpecArgs {
    /// Snapshot directory
    snapshot: PathBuf,
}

impl SpecArgs {
    fn run(self) -> Result<()> {
        snapshot::Snapshot::new(self.snapshot)
            .generate_spec()
            .map(drop)
    }
}

/// Arguments of `fixtures`.
#[derive(Args)]
struct FixturesArgs {
    /// Snapshot directory
    snapshot: PathBuf,
    /// Output directory
    #[arg(long, default_value = LIBRARY_FIXTURES)]
    out: PathBuf,
}

impl FixturesArgs {
    fn run(self, scrubber: &Scrubber) -> Result<()> {
        let snapshot = snapshot::Snapshot::new(&self.snapshot);
        let captures = snapshot.captures()?;
        let (routes, router_log) = (snapshot.routes(), snapshot.router_log());
        let exported = fixtures::run(&fixtures::FixturesOpts {
            captures: &captures,
            routes: Some(&routes),
            router_log: Some(&router_log),
            out: &self.out,
            scrubber,
        })?;
        eprintln!("{} fixtures → {}", exported.written, self.out.display());
        for name in &exported.removed {
            eprintln!("removed {name}: the snapshot no longer has this exchange");
        }
        Ok(())
    }
}

/// Arguments of `privacy-check`.
#[derive(Args)]
struct PrivacyCheckArgs {
    /// Files or directories to check (default: the snapshots, the docker-sandbox fixtures and the
    /// files of the published docker-sandbox crate)
    paths: Vec<PathBuf>,
}

impl PrivacyCheckArgs {
    fn run(self, scrubber: &Scrubber) -> Result<()> {
        let root = Path::new(WORKSPACE);
        let paths = if self.paths.is_empty() {
            let mut paths = vec![PathBuf::from(SNAPSHOTS), PathBuf::from(LIBRARY_FIXTURES)];
            for name in PUBLISHED_CRATES {
                let published = privacy::published_files(root, name)?;
                eprintln!("{} files published with {name}", published.len());
                paths.extend(published);
            }
            paths
        } else {
            self.paths
        };
        // The public contact of the maintainer is published on purpose.
        let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
            .context("reading the root manifest")?;
        let allowed = privacy::declared_contacts(&manifest);
        let findings = privacy::check_allowing(&paths, scrubber, &allowed)?;
        for finding in &findings {
            println!(
                "{}:{}: {}",
                finding.file.display(),
                finding.line,
                finding.what
            );
        }
        if findings.is_empty() {
            eprintln!("no personal data found");
            Ok(())
        } else {
            bail!("{} findings", findings.len())
        }
    }
}

/// Arguments of `scrub`.
#[derive(Args)]
struct ScrubArgs {
    /// File to read
    input: PathBuf,
    /// File to write
    output: PathBuf,
}

impl ScrubArgs {
    fn run(self, scrubber: &Scrubber) -> Result<()> {
        let text = std::fs::read_to_string(&self.input)
            .with_context(|| self.input.display().to_string())?;
        let mut out: String = text
            .lines()
            .map(|line| scrubber.line(line))
            .collect::<Vec<_>>()
            .join("\n");
        if text.ends_with('\n') {
            out.push('\n');
        }
        write_text(&self.output, &out)
    }
}

/// Arguments of `scenarios`.
#[derive(Args)]
struct ScenariosArgs {
    /// Embedded suite to replay
    #[arg(long, conflicts_with = "file", required_unless_present_any = ["file", "list"])]
    suite: Option<String>,
    /// Suite file to replay instead of an embedded suite
    #[arg(long)]
    file: Option<PathBuf>,
    /// List the embedded suites and exit
    #[arg(long)]
    list: bool,
    /// Output directory (`<suite>.jsonl` capture and `<suite>.json` results)
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/audit/scenarios"))]
    out: PathBuf,
    /// Only run these scenarios (comma-separated)
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// Temporarily replace the real socket with the spy
    #[arg(long)]
    swap: bool,
    /// Do not redact secret values (only with dummy values!)
    #[arg(long)]
    no_redact: bool,
    /// `sbx` executable
    #[arg(long, default_value = "sbx")]
    sbx: String,
    /// Timeout of each scenario, in seconds
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,
}

impl ScenariosArgs {
    async fn run(self, sock: &Path, scrubber: Arc<Scrubber>) -> Result<()> {
        if self.list {
            for (name, _) in assets::SUITES {
                let suite = assets::suite(name)?;
                println!(
                    "{name:<10} {:>3} scenarios  {}",
                    suite.scenarios.len(),
                    suite.description
                );
            }
            return Ok(());
        }
        let suite = match (&self.suite, &self.file) {
            (Some(name), _) => assets::suite(name)?,
            (None, Some(file)) => assets::suite_file(file)?,
            (None, None) => bail!("give --suite or --file"),
        };
        let work = assets::WorkDir::prepare(&assets::WorkDir::default_root())?;
        let capture = self.out.join(format!("{}.jsonl", suite.name));
        // Like `audit`, a replay leaves the daemon as it found it: settings are
        // restored, and the sandboxes and images it created are removed.
        let daemon = client::SockClient::new(sock);
        let baseline = snapshot::DaemonState::read(&daemon, &self.sbx).await?;
        let run = scenarios::run(scenarios::RunOpts {
            sbx: &self.sbx,
            upstream: sock,
            work: &work,
            capture: &capture,
            suite: &suite,
            only: &self.only,
            swap: self.swap,
            redact: !self.no_redact,
            scrubber: Arc::clone(&scrubber),
            timeout: Duration::from_secs(self.timeout_secs),
        });
        // On Ctrl-C the run is dropped when leaving the select: its guards stop
        // the spy and, in swap mode, restore the real socket. The daemon is
        // restored in every case.
        let results = tokio::select! {
            finished = run => finished,
            _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("interrupted")),
        };
        let after = snapshot::DaemonState::read(&daemon, &self.sbx).await?;
        baseline.restore(&after, &daemon).await;
        let results = results?;
        let mut value = serde_json::to_value(&results)?;
        scrubber.value(&mut value);
        write_json(&self.out.join(format!("{}.json", suite.name)), &value)
    }
}

/// Arguments of `routes`.
#[derive(Args)]
struct RoutesArgs {
    /// `sbx` executable, whose literals are scanned
    #[arg(long, default_value = "sbx")]
    sbx: String,
    /// Inventory file
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/audit/routes.json"))]
    out: PathBuf,
}

impl RoutesArgs {
    async fn run(self, sock: &Path, scrubber: &Scrubber) -> Result<()> {
        let binary = routes::find_sbx(&self.sbx)?;
        let inventory = routes::inventory(sock, &binary).await?;
        for (route, allow) in &inventory.routes {
            println!("{route:<56} {allow}");
        }
        eprintln!(
            "{} routes ({} candidates, {} requests)",
            inventory.routes.len(),
            inventory.candidates,
            inventory.requests
        );
        let mut value = serde_json::to_value(&inventory)?;
        scrubber.value(&mut value);
        write_json(&self.out, &value)
    }
}

/// Arguments of `router-log`.
#[derive(Args)]
struct RouterLogArgs {
    /// Daemon log (default: `daemon.log` next to the socket)
    #[arg(long)]
    log: Option<PathBuf>,
    /// Output file
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/audit/router-log.json"))]
    out: PathBuf,
}

impl RouterLogArgs {
    fn run(self, sock: &Path, scrubber: &Scrubber) -> Result<()> {
        let log = self
            .log
            .unwrap_or_else(|| sock.with_file_name("daemon.log"));
        let routes: std::collections::BTreeMap<String, String> = spec::router_log_routes(&log, 0)?
            .into_iter()
            .map(|(route, methods)| (route, methods.into_iter().collect::<Vec<_>>().join(", ")))
            .collect();
        eprintln!("{} router patterns", routes.len());
        let mut value = serde_json::json!({ "routes": routes });
        scrubber.value(&mut value);
        write_json(&self.out, &value)
    }
}

/// Arguments of `probe`.
#[derive(Args)]
struct ProbeArgs {
    /// Existing sandbox used for the {sandbox} routes
    #[arg(long, default_value = "audit-a")]
    sandbox: String,
    /// File of additional routes, one per line (default: the embedded list)
    #[arg(long)]
    wordlist: Option<PathBuf>,
    /// Report file
    #[arg(long)]
    out: Option<PathBuf>,
}

impl ProbeArgs {
    async fn run(self, sock: &Path) -> Result<()> {
        let wordlist = match &self.wordlist {
            Some(path) => {
                std::fs::read_to_string(path).with_context(|| path.display().to_string())?
            }
            None => assets::PROBE_ROUTES.to_owned(),
        };
        let results = probe::run(sock, &self.sandbox, Some(&wordlist)).await?;
        for result in results.iter().filter(|r| r.method == "GET") {
            let status = result
                .status
                .map_or_else(|| "—".to_owned(), |s| s.to_string());
            println!(
                "{:<6} {:<48} {:>4}  {}",
                result.method, result.route, status, result.verdict
            );
        }
        match &self.out {
            Some(path) => write_json(path, &results),
            None => Ok(()),
        }
    }
}

/// Arguments of `perms`.
#[derive(Args)]
struct PermsArgs {
    /// Report file
    #[arg(long)]
    out: Option<PathBuf>,
}

impl PermsArgs {
    async fn run(self, sock: &Path) -> Result<()> {
        let report = perms::run(sock).await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        match &self.out {
            Some(path) => write_json(path, &report),
            None => Ok(()),
        }
    }
}

/// Arguments of `spy`.
#[derive(Args)]
struct SpyArgs {
    /// Socket the spy listens on
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/audit/spy/spy.sock"))]
    listen: PathBuf,
    /// Capture file (JSON lines, appended)
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/audit/spy/capture.jsonl"))]
    capture: PathBuf,
    /// Tag recorded with every exchange
    #[arg(long, default_value = "manual")]
    tag: String,
    /// File re-read on every connection to get the tag (takes precedence over --tag)
    #[arg(long)]
    tag_file: Option<PathBuf>,
    /// Do not redact secret values (only with dummy values!)
    #[arg(long)]
    no_redact: bool,
}

impl SpyArgs {
    async fn run(self, sock: &Path, scrubber: Arc<Scrubber>) -> Result<()> {
        let state = spy::SpyState::new(&self.capture, !self.no_redact, scrubber)?;
        state.set_tag(&self.tag);
        if let Some(file) = self.tag_file {
            state.set_tag_file(file);
        }
        if let Some(parent) = self.listen.parent() {
            std::fs::create_dir_all(parent)?;
        }
        eprintln!("export DOCKER_SANDBOXES_API={}", self.listen.display());
        tokio::select! {
            served = spy::serve(self.listen.clone(), sock.to_path_buf(), state) => served?,
            _ = tokio::signal::ctrl_c() => eprintln!("\n[spy] stopped"),
        }
        let _ = std::fs::remove_file(&self.listen);
        Ok(())
    }
}

/// Arguments of `summarize`.
#[derive(Args)]
struct SummarizeArgs {
    /// Capture file (`<suite>.jsonl`)
    capture: PathBuf,
    /// Output directory
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/audit/summary"))]
    out: PathBuf,
    /// Sandbox names to replace with {sandbox} in routes
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "audit-a,audit-b,audit-env,audit-k,audit-r,audit-w"
    )]
    names: Vec<String>,
}

/// Arguments of `requests`.
#[derive(Args)]
struct RequestsArgs {
    /// Capture file (`<suite>.jsonl`), e.g. `target/audit/scenarios/agents.jsonl`
    capture: PathBuf,
    /// Only these scenarios (comma-separated)
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// Include the reads (`GET`)
    #[arg(long)]
    reads: bool,
}

/// Arguments of `state`.
#[derive(Args)]
struct StateArgs {
    /// Record the current state into --file
    #[arg(long, conflicts_with = "check")]
    save: bool,
    /// Compare the current state with --file; fails when it differs
    #[arg(long)]
    check: bool,
    /// Recorded state
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/audit/state.json"))]
    file: PathBuf,
    /// `sbx` executable, for the secrets and MCP servers it lists
    #[arg(long, default_value = "sbx")]
    sbx: String,
}

impl SummarizeArgs {
    fn run(self) -> Result<()> {
        let catalog = summarize::run(&self.capture, &self.out, &self.names)?;
        eprintln!(
            "{} distinct routes → {}/catalog.json, {}/matrix.md",
            catalog.routes.len(),
            self.out.display(),
            self.out.display()
        );
        Ok(())
    }
}

/// Arguments of `kit-compile`.
#[derive(Args)]
struct KitCompileArgs {
    /// Kit directories (mixins) composed with the base kit
    #[arg(required = true)]
    kits: Vec<PathBuf>,
    /// Base kit: a built-in agent (shell, claude, …) or a kit reference
    #[arg(long, default_value = "shell")]
    base: String,
    /// Kit argument, passed to `sbx create --kit-arg`
    #[arg(long = "kit-arg")]
    kit_args: Vec<String>,
    /// Output directory (default: `target/audit/kits/<first kit name>`)
    #[arg(long)]
    out: Option<PathBuf>,
    /// Do not replay the package through the socket after compiling it
    #[arg(long)]
    no_verify: bool,
    /// `sbx` executable
    #[arg(long, default_value = "sbx")]
    sbx: String,
    /// Timeout of the compilation, in seconds
    #[arg(long, default_value_t = 600)]
    timeout_secs: u64,
}

impl KitCompileArgs {
    async fn run(self, sock: &Path) -> Result<()> {
        let out = self.out.clone().unwrap_or_else(|| {
            let name = self.kits[0]
                .canonicalize()
                .ok()
                .and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "kit".to_owned());
            Path::new(OUTPUT).join("kits").join(name)
        });
        kitc::run(&kitc::KitcOpts {
            sbx: &self.sbx,
            socket: sock,
            base: &self.base,
            kits: &self.kits,
            kit_args: &self.kit_args,
            out: &out,
            verify: !self.no_verify,
            timeout: Duration::from_secs(self.timeout_secs),
        })
        .await
    }
}

/// Arguments of `kit-forge`.
#[derive(Args)]
struct KitForgeArgs {
    /// Kit directory of a v3 mixin, its `spec.yaml` written in the JSON subset of YAML
    kit: PathBuf,
    /// Package compiled by `kit-compile` whose base kit is reused
    #[arg(
        long,
        required_unless_present = "base_image",
        conflicts_with = "base_image"
    )]
    base_package: Option<PathBuf>,
    /// Published base kit rebuilt through the socket, e.g. `docker.io/sbx/shell:latest`
    #[arg(long)]
    base_image: Option<String>,
    /// Output directory (default: `target/audit/kits/<kit name>-forged`)
    #[arg(long)]
    out: Option<PathBuf>,
    /// Top-level fields left out of the mixin artifact, e.g. `commands,v3Selection`
    #[arg(long, value_delimiter = ',')]
    omit: Vec<String>,
    /// Top-level fields left out of the base kit artifact
    #[arg(long, value_delimiter = ',')]
    omit_base: Vec<String>,
    /// Value of an argument the base kit declares, as `name=value` (with `--base-image`)
    #[arg(long = "base-arg", requires = "base_image")]
    base_args: Vec<String>,
    /// Package compiled by the CLI for the same kits, to compare the artifacts
    #[arg(long)]
    compare: Option<PathBuf>,
    /// Forge with the `KitForge` of the library instead of this experiment
    #[arg(long, conflicts_with_all = ["omit", "omit_base"])]
    library: bool,
}

impl KitForgeArgs {
    async fn run(self, sock: &Path) -> Result<()> {
        let out = self
            .out
            .clone()
            .unwrap_or_else(|| kitforge::default_out(Path::new(OUTPUT), &self.kit));
        let base = match (&self.base_package, &self.base_image) {
            (Some(package), _) => kitforge::BaseSource::Package(package),
            (None, Some(reference)) => kitforge::BaseSource::Image {
                reference,
                socket: sock,
                args: kitc::parse_kit_args(&self.base_args)?,
            },
            (None, None) => bail!("--base-package or --base-image is required"),
        };
        let opts = kitforge::ForgeOpts {
            kit: &self.kit,
            base,
            out: &out,
            omit: &self.omit,
            omit_base: &self.omit_base,
            compare: self.compare.as_deref(),
        };
        if self.library {
            kitforge::run_library(&opts, sock).await
        } else {
            kitforge::run(&opts).await
        }
    }
}

/// Arguments of `replay`.
#[derive(Args)]
struct ReplayArgs {
    /// Capture file (`<suite>.jsonl`)
    capture: PathBuf,
    /// Connection id of the request in the capture
    #[arg(long)]
    conn: u64,
    /// Sequence number of the request on its connection
    #[arg(long)]
    seq: u64,
    /// Print the request without sending it
    #[arg(long)]
    dry_run: bool,
}

/// Arguments of `raw`.
#[derive(Args)]
struct RawArgs {
    /// HTTP method
    method: String,
    /// Request target, query string included
    path: String,
    /// Literal JSON body, or @file
    #[arg(long)]
    data: Option<String>,
    /// Additional header, as `Name: value` (repeatable)
    #[arg(long = "header", short = 'H')]
    headers: Vec<String>,
    /// Upgrade the connection and bridge it to the terminal
    #[arg(long)]
    upgrade: bool,
    /// Demultiplex the stream as Docker stdcopy frames
    #[arg(long)]
    stdcopy: bool,
}

impl RawArgs {
    async fn run(self, sock: &Path) -> Result<()> {
        let headers = raw::parse_headers(&self.headers)?;
        let body = raw::load_data(self.data.as_deref())?;
        raw::send(
            sock,
            &self.method,
            &self.path,
            headers,
            body,
            self.upgrade,
            self.stdcopy,
        )
        .await
    }
}

/// Writes `value` as pretty JSON to `path`, creating parent directories.
fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_bytes(path, &bytes)
}

/// Writes `text` to `path`, creating parent directories.
fn write_text(path: &Path, text: &str) -> Result<()> {
    write_bytes(path, text.as_bytes())
}

/// Writes `bytes` to `path`, creating parent directories.
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes).with_context(|| path.display().to_string())?;
    eprintln!("→ {}", path.display());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let sock = match cli.sock {
        Some(path) => path,
        None => docker_sandbox::default_socket_path()
            .context("no socket given and neither DOCKER_SANDBOXES_API nor HOME is set")?,
    };
    let scrubber = Arc::new(Scrubber::from_env(&cli.private));
    cli.cmd.run(&sock, scrubber).await
}
