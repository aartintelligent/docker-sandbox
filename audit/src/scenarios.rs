//! Scenario suites replayed behind the spy proxy.
//!
//! A [`Suite`] is an ordered list of [`Scenario`]s. Before each scenario,
//! [`run`] sets the spy tag to the scenario name, so that every exchange in the
//! capture can be attributed to the step that caused it. A scenario is one of:
//!
//! - a **command**: `sbx` with arguments (or another program, such as
//!   `{audit}`), optionally fed through standard input or run inside a
//!   pseudo-terminal;
//! - a **request** sent straight to the socket through the spy, for routes the
//!   CLI rarely or never calls; it may upgrade the connection (`exec`, `ssh`)
//!   or be **held** open (`session`) until a later **release** step.
//!
//! Strings may use these placeholders: `{work}` (scratch directory),
//! `{fixtures}` (scenario input files), `{kits}` (kit sources), `{spy}` (socket
//! the scenarios talk to) and `{audit}` (this executable).
//!
//! Two redirection modes are supported:
//!
//! - default: the spy listens on its own socket and commands are pointed at it
//!   through the `DOCKER_SANDBOXES_API` environment variable;
//! - swap (`--swap`): the real socket is temporarily moved aside and the spy
//!   listens at its original path, for CLI versions that ignore
//!   `DOCKER_SANDBOXES_API` (see `SwapGuard`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::assets::WorkDir;
use crate::client::{SockClient, preview};
use crate::privacy::Scrubber;
use crate::spy::{self, SpyState};

/// Size kept of the standard output and error of each scenario.
const OUTPUT_LIMIT: usize = 4000;
/// Time left after each scenario for its last connections to close.
const GRACE: Duration = Duration::from_millis(300);
/// How long an upgraded stream is read when the scenario does not say.
const DEFAULT_STREAM_WAIT: Duration = Duration::from_secs(2);

/// An ordered list of scenarios, read from JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Suite {
    /// Name of the suite, also the name of its capture file.
    pub name: String,
    /// What the suite covers.
    pub description: String,
    /// Scenarios, in order: later ones use what earlier ones created, and the
    /// last ones remove it.
    pub scenarios: Vec<Scenario>,
}

/// One step of a suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Scenario {
    /// Unique name within the suite, used as the spy tag and matched by `--only`.
    pub name: String,
    /// Free-form grouping label (`discovery`, `lifecycle`, `policy`, …).
    pub category: String,
    /// What the scenario does.
    #[serde(flatten)]
    pub step: Step,
    /// Whether the step changes daemon state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mutates: bool,
    /// Whether the step is expected to fail, e.g. to document an error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub expect_error: bool,
    /// Free-text remark for the auditor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The action of a [`Scenario`], recognized by its field: `request`,
/// `release` or `args`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Step {
    /// A request sent straight to the socket.
    Request {
        /// The request, boxed: it is much larger than the other steps.
        request: Box<RequestStep>,
    },
    /// Closes a connection held by an earlier request.
    Release {
        /// The `hold` key of that request.
        release: String,
    },
    /// A command.
    Command(CommandStep),
}

/// A command run by a scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CommandStep {
    /// Program to run instead of `sbx`, e.g. `{audit}`. Only `sbx` gets
    /// `--no-input` prepended to its arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// Arguments, without the program itself.
    pub args: Vec<String>,
    /// Data written to standard input. Without it, the command reads from
    /// `/dev/null` (or from an idle terminal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// Runs the command inside a pseudo-terminal of this size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<Terminal>,
}

/// Size of a pseudo-terminal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct Terminal {
    /// Number of rows.
    pub rows: u16,
    /// Number of columns.
    pub cols: u16,
}

/// A request sent straight to the socket by a scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RequestStep {
    /// HTTP method.
    pub method: String,
    /// Request target, query string included.
    pub path: String,
    /// JSON body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    /// File whose content is the body, sent as JSON when it parses as JSON
    /// and as `application/octet-stream` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_file: Option<String>,
    /// Literal replacements applied to the body read from `body_file`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub replace: BTreeMap<String, String>,
    /// JSON pointer selecting the part of the JSON `body_file` that is sent,
    /// e.g. `/kit_artifacts/1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    /// Key under which the body read from `body_file` is wrapped in a JSON
    /// object, e.g. `kit_artifact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap: Option<String>,
    /// Media type of the body read from `body_file`, instead of JSON or
    /// `application/octet-stream`, e.g. `application/x-tar`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Upgrades the connection with this protocol (`tcp`, `ssh`) and reads
    /// the stream for `wait_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<String>,
    /// Data written to the upgraded stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// How long the upgraded stream is read, in milliseconds (default 2000).
    /// On a plain request, reads the body for that long at most, for streaming
    /// routes whose body never ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
    /// Keeps the connection open under this key until a `release` step, for
    /// streaming routes such as `session`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<String>,
}

/// How a scenario ended, compared with what it expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    /// The step succeeded, as expected.
    Ok,
    /// The step failed, as expected.
    ExpectedError,
    /// The step failed although it was expected to succeed.
    Failed,
    /// The step succeeded although it was expected to fail.
    UnexpectedSuccess,
}

/// Outcome of one scenario. Results are also logged in the capture as
/// `scenario_result` events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ScenarioResult {
    /// Scenario name.
    pub name: String,
    /// Scenario category.
    pub category: String,
    /// What was run, placeholders substituted: the command line or the request
    /// line.
    pub action: String,
    /// Exit code of a command; `None` for requests, and for commands that could
    /// not run, timed out or were killed by a signal.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// HTTP status of a request.
    #[serde(default)]
    pub status: Option<u16>,
    /// Whether the step was killed because it exceeded the timeout.
    #[serde(default)]
    pub timed_out: bool,
    /// Outcome compared with the expectation.
    pub verdict: Verdict,
    /// Wall-clock duration, without the grace period left for the last
    /// connections to close.
    pub duration_ms: u64,
    /// Connections the spy relayed to the daemon during the scenario.
    pub connections: u64,
    /// Standard output of a command (merged terminal output with a
    /// pseudo-terminal), or the body of a request, truncated.
    pub stdout: String,
    /// Standard error of a command, or why the step could not complete,
    /// truncated.
    pub stderr: String,
}

/// Temporarily moves the real socket aside so that the spy can take its
/// place, for CLI versions that ignore `DOCKER_SANDBOXES_API`.
///
/// A listening Unix socket is identified by its inode, not by its path:
/// renaming the path does not cut the daemon off. Dropping the guard removes
/// whatever sits at the original path (the spy socket) and renames the real
/// socket back.
struct SwapGuard {
    /// Original path of the real socket, taken over by the spy.
    original: PathBuf,
    /// Temporary path of the real socket.
    moved: PathBuf,
}

impl SwapGuard {
    /// Moves the real socket at `original` to its temporary path.
    fn new(original: &Path) -> Result<Self> {
        let moved = original.with_extension("sock.audit-orig");
        std::fs::rename(original, &moved).context("renaming the real socket")?;
        Ok(Self {
            original: original.to_path_buf(),
            moved,
        })
    }
}

impl Drop for SwapGuard {
    /// Removes the spy socket and restores the real socket at its original
    /// path, printing the manual recovery command if the restore fails.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.original);
        if let Err(e) = std::fs::rename(&self.moved, &self.original) {
            eprintln!(
                "!! MANUAL RESTORE REQUIRED: mv {} {} ({e})",
                self.moved.display(),
                self.original.display()
            );
        } else {
            eprintln!("[swap] real socket restored");
        }
    }
}

/// Options for [`run`].
pub(crate) struct RunOpts<'a> {
    /// Path or name of the `sbx` binary.
    pub sbx: &'a str,
    /// Path of the real `sandboxd` socket.
    pub upstream: &'a Path,
    /// Scratch directory, prepared with [`WorkDir::prepare`].
    pub work: &'a WorkDir,
    /// Capture file the spy appends to.
    pub capture: &'a Path,
    /// Suite to replay.
    pub suite: &'a Suite,
    /// Names of the scenarios to run; empty means all of them.
    pub only: &'a [String],
    /// Use swap mode (see `SwapGuard`) instead of `DOCKER_SANDBOXES_API`.
    pub swap: bool,
    /// Redact secret values in the capture.
    pub redact: bool,
    /// Scrubber applied to everything the capture records.
    pub scrubber: Arc<Scrubber>,
    /// Maximum duration of one scenario.
    pub timeout: Duration,
}

/// Replays the selected scenarios of a suite behind the spy and returns their
/// outcomes. A failing scenario does not stop the run.
///
/// # Errors
///
/// Fails if the capture file cannot be created, if the spy cannot start or if
/// swap mode cannot move the real socket.
pub(crate) async fn run(o: RunOpts<'_>) -> Result<Vec<ScenarioResult>> {
    let st = SpyState::new(o.capture, o.redact, Arc::clone(&o.scrubber))?;

    // Default mode: the spy listens on its own socket and commands are
    // redirected to it through an environment variable. Swap mode: the spy
    // takes the place of the real socket; the guard restores it on drop.
    let (_guard, listen, upstream) = if o.swap {
        let guard = SwapGuard::new(o.upstream)?;
        let moved = guard.moved.clone();
        (Some(guard), o.upstream.to_path_buf(), moved)
    } else {
        (None, o.work.spy_socket(), o.upstream.to_path_buf())
    };

    let mut spy_task = SpyTask {
        handle: tokio::spawn(spy::serve(listen.clone(), upstream, st.clone())),
        socket: (!o.swap).then(|| listen.clone()),
    };
    // Give the spy time to bind its socket before the first step runs.
    tokio::time::sleep(Duration::from_millis(200)).await;
    if spy_task.handle.is_finished() {
        return match (&mut spy_task.handle).await {
            Ok(Err(error)) => Err(error.context("the spy could not start")),
            _ => Err(anyhow::anyhow!("the spy stopped unexpectedly")),
        };
    }

    let placeholders = Placeholders::new(o.work, &listen)?;
    let mut runner = StepRunner {
        sbx: o.sbx,
        api: (!o.swap).then(|| listen.clone()),
        spy: SockClient::new(&listen),
        placeholders,
        held: HashMap::new(),
        timeout: o.timeout,
    };

    let mut results = vec![];
    for scenario in o
        .suite
        .scenarios
        .iter()
        .filter(|s| o.only.is_empty() || o.only.contains(&s.name))
    {
        st.set_tag(&scenario.name);
        st.event(json!({"kind": "scenario_start", "tag": scenario.name, "step": scenario.step}));
        let before = st.conns.load(Ordering::SeqCst);
        let started = Instant::now();
        let outcome = runner.run(&scenario.step).await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tokio::time::sleep(GRACE).await;

        let result = ScenarioResult {
            name: scenario.name.clone(),
            category: scenario.category.clone(),
            verdict: verdict(outcome.succeeded(), scenario.expect_error),
            action: outcome.action,
            exit_code: outcome.exit_code,
            status: outcome.status,
            timed_out: outcome.timed_out,
            duration_ms,
            connections: st.conns.load(Ordering::SeqCst) - before,
            stdout: preview(outcome.stdout.as_bytes(), OUTPUT_LIMIT),
            stderr: preview(outcome.stderr.as_bytes(), OUTPUT_LIMIT),
        };
        st.event(json!({"kind": "scenario_result", "tag": scenario.name, "result": result}));
        eprintln!(
            "[{:>6}ms] {:<28} {:<20} conns={}",
            result.duration_ms,
            result.name,
            format!("{:?}", result.verdict),
            result.connections
        );
        results.push(result);
    }
    runner.held.clear();

    if results.is_empty() {
        eprintln!("\n!! No scenario matched the --only filter.");
    } else if !o.swap && results.iter().all(|r| r.connections == 0) {
        eprintln!(
            "\n!! The spy saw no connection: the CLI probably ignores \
             DOCKER_SANDBOXES_API. Run again with --swap."
        );
    }
    drop(spy_task);
    Ok(results)
}

/// The verdict of a step that succeeded or not, given its expectation.
fn verdict(succeeded: bool, expect_error: bool) -> Verdict {
    match (succeeded, expect_error) {
        (true, false) => Verdict::Ok,
        (false, true) => Verdict::ExpectedError,
        (false, false) => Verdict::Failed,
        (true, true) => Verdict::UnexpectedSuccess,
    }
}

/// Substitutes the placeholders of a suite.
struct Placeholders {
    /// `(placeholder, value)` pairs.
    pairs: Vec<(&'static str, String)>,
}

impl Placeholders {
    /// The placeholders of a run in `work`, talking to `spy`.
    fn new(work: &WorkDir, spy: &Path) -> Result<Self> {
        let exe = std::env::current_exe().context("locating the audit executable")?;
        let path = |p: &Path| p.display().to_string();
        Ok(Self {
            pairs: vec![
                ("{fixtures}", path(&work.fixtures())),
                ("{kits}", path(&work.kits())),
                ("{work}", path(&work.root)),
                ("{spy}", path(spy)),
                ("{audit}", path(&exe)),
            ],
        })
    }

    /// Substitutes the placeholders in `text`.
    fn text(&self, text: &str) -> String {
        self.pairs
            .iter()
            .fold(text.to_owned(), |acc, (from, to)| acc.replace(from, to))
    }

    /// Substitutes the placeholders in every string of `value`.
    fn value(&self, value: &mut Value) {
        match value {
            Value::String(s) => *s = self.text(s),
            Value::Array(items) => items.iter_mut().for_each(|v| self.value(v)),
            Value::Object(map) => map.values_mut().for_each(|v| self.value(v)),
            _ => {}
        }
    }
}

/// What a step produced.
#[derive(Debug, Default)]
struct StepOutcome {
    /// What was run.
    action: String,
    /// Exit code of a command.
    exit_code: Option<i32>,
    /// HTTP status of a request.
    status: Option<u16>,
    /// Whether the step timed out.
    timed_out: bool,
    /// Standard output or response body.
    stdout: String,
    /// Standard error or failure reason.
    stderr: String,
}

impl StepOutcome {
    /// A step that could not complete.
    fn failed(action: String, reason: impl std::fmt::Display) -> Self {
        Self {
            action,
            stderr: reason.to_string(),
            ..Self::default()
        }
    }

    /// Whether the step succeeded: exit code 0, or a status below 400.
    fn succeeded(&self) -> bool {
        self.exit_code == Some(0) || self.status.is_some_and(|s| s < 400)
    }
}

/// Runs the steps of a suite.
struct StepRunner<'a> {
    /// `sbx` executable.
    sbx: &'a str,
    /// Socket commands are pointed at, in default mode.
    api: Option<PathBuf>,
    /// Client of the socket the scenarios talk to.
    spy: SockClient,
    /// Placeholder values.
    placeholders: Placeholders,
    /// Connections held open, by `hold` key. Dropping a task closes its
    /// connection.
    held: HashMap<String, HeldConnection>,
    /// Maximum duration of one step.
    timeout: Duration,
}

/// A connection held open by a `hold` request; closed when dropped.
struct HeldConnection(tokio::task::JoinHandle<()>);

impl Drop for HeldConnection {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl StepRunner<'_> {
    /// Runs one step.
    async fn run(&mut self, step: &Step) -> StepOutcome {
        match step {
            Step::Command(command) => self.command(command).await,
            Step::Request { request } => self.request(request).await,
            Step::Release { release } => {
                let action = format!("release {release}");
                if self.held.remove(release).is_some() {
                    StepOutcome {
                        action,
                        exit_code: Some(0),
                        ..StepOutcome::default()
                    }
                } else {
                    StepOutcome::failed(action, "no connection held under this key")
                }
            }
        }
    }

    /// Runs a command step.
    async fn command(&self, step: &CommandStep) -> StepOutcome {
        let program = step
            .program
            .as_deref()
            .map_or_else(|| self.sbx.to_owned(), |p| self.placeholders.text(p));
        let mut args: Vec<String> = Vec::new();
        if step.program.is_none() {
            args.push("--no-input".to_owned());
        }
        args.extend(step.args.iter().map(|a| self.placeholders.text(a)));
        let action = std::iter::once(program.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");

        if let Some(terminal) = step.tty {
            let (stdin, api, timeout) = (step.stdin.clone(), self.api.clone(), self.timeout);
            let size = (terminal.rows, terminal.cols);
            let run = tokio::task::spawn_blocking(move || {
                crate::pty::run(
                    &program,
                    &args,
                    stdin.as_deref(),
                    api.as_deref(),
                    size,
                    timeout,
                )
            });
            return match run.await {
                Ok(Ok(outcome)) => StepOutcome {
                    action,
                    exit_code: outcome.exit_code,
                    timed_out: outcome.timed_out,
                    stdout: outcome.output,
                    ..StepOutcome::default()
                },
                Ok(Err(error)) => StepOutcome::failed(action, format!("{error:#}")),
                Err(error) => StepOutcome::failed(action, error),
            };
        }
        run_command(
            action,
            &program,
            &args,
            step.stdin.as_deref(),
            self.api.as_deref(),
            self.timeout,
        )
        .await
    }

    /// Sends a request step through the spy.
    async fn request(&mut self, step: &RequestStep) -> StepOutcome {
        let path = self.placeholders.text(&step.path);
        let action = format!("{} {path}", step.method);
        if crate::probe::is_forbidden(&path) {
            return StepOutcome::failed(action, "refused: destructive daemon route");
        }
        let request = match self.build(step, &path) {
            Ok(request) => request,
            Err(error) => return StepOutcome::failed(action, format!("{error:#}")),
        };

        if let Some(key) = &step.hold {
            let client = self.spy.clone();
            let task = tokio::spawn(async move {
                let _ = client.send(request).await;
            });
            self.held.insert(key.clone(), HeldConnection(task));
            // Leave the request time to reach the daemon.
            tokio::time::sleep(GRACE).await;
            return StepOutcome {
                action,
                exit_code: Some(0),
                stdout: format!("held as {key}"),
                ..StepOutcome::default()
            };
        }

        if step.upgrade.is_some() {
            let wait = step
                .wait_ms
                .map_or(DEFAULT_STREAM_WAIT, Duration::from_millis);
            return match self.stream(request, step.stdin.as_deref(), wait).await {
                Ok(output) => StepOutcome {
                    action,
                    status: Some(101),
                    stdout: preview(&output, OUTPUT_LIMIT),
                    ..StepOutcome::default()
                },
                Err(error) => StepOutcome::failed(action, format!("{error:#}")),
            };
        }

        let sent = match step.wait_ms {
            Some(wait) => Ok(self
                .spy
                .send_for(request, Duration::from_millis(wait))
                .await),
            None => tokio::time::timeout(self.timeout, self.spy.send(request)).await,
        };
        match sent {
            Ok(Ok(response)) => StepOutcome {
                action,
                status: Some(response.status.as_u16()),
                stdout: preview(&response.body, OUTPUT_LIMIT),
                ..StepOutcome::default()
            },
            Ok(Err(error)) => StepOutcome::failed(action, format!("{error:#}")),
            Err(_) => StepOutcome {
                timed_out: true,
                ..StepOutcome::failed(action, "timeout")
            },
        }
    }

    /// Builds the HTTP request of a step.
    fn build(
        &self,
        step: &RequestStep,
        path: &str,
    ) -> Result<hyper::Request<http_body_util::Full<Bytes>>> {
        let mut headers = Vec::new();
        let body = match (&step.body, &step.body_file) {
            (Some(_), Some(_)) => bail!("a request has either a body or a body_file"),
            (Some(body), None) => {
                let mut body = body.clone();
                self.placeholders.value(&mut body);
                Some(Bytes::from(serde_json::to_vec(&body)?))
            }
            (None, Some(file)) => {
                let file = self.placeholders.text(file);
                let mut bytes = std::fs::read(&file).with_context(|| format!("reading {file}"))?;
                if let Ok(mut text) = String::from_utf8(bytes.clone()) {
                    for (from, to) in &step.replace {
                        text = text.replace(from, to);
                    }
                    bytes = text.into_bytes();
                }
                if step.pointer.is_some() || step.wrap.is_some() {
                    bytes = select_json(&bytes, step, &file)?;
                }
                if let Some(content_type) = &step.content_type {
                    headers.push(("content-type".to_owned(), content_type.clone()));
                } else if serde_json::from_slice::<Value>(&bytes).is_err() {
                    headers.push((
                        "content-type".to_owned(),
                        "application/octet-stream".to_owned(),
                    ));
                }
                Some(Bytes::from(bytes))
            }
            (None, None) => None,
        };
        if let Some(protocol) = &step.upgrade {
            headers.push(("upgrade".to_owned(), protocol.clone()));
        }
        SockClient::build(&step.method, path, &headers, body)
    }

    /// Upgrades the connection, writes `stdin`, then reads the stream for
    /// `wait`.
    async fn stream(
        &self,
        request: hyper::Request<http_body_util::Full<Bytes>>,
        stdin: Option<&str>,
        wait: Duration,
    ) -> Result<Vec<u8>> {
        let mut stream = self.spy.upgrade(request).await?;
        if let Some(input) = stdin {
            stream.write_all(input.as_bytes()).await?;
        }
        let mut output = Vec::new();
        let mut buffer = [0_u8; 8192];
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Ok(read)) = tokio::time::timeout_at(deadline, stream.read(&mut buffer)).await {
            if read == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..read]);
        }
        let _ = stream.shutdown().await;
        Ok(output)
    }
}

/// Runs `program` with `args`, feeding it `stdin` when given, and kills it once
/// `timeout` expires. With `api`, the command is pointed at that socket through
/// `DOCKER_SANDBOXES_API`.
async fn run_command(
    action: String,
    program: &str,
    args: &[String],
    stdin: Option<&str>,
    api: Option<&Path>,
    timeout: Duration,
) -> StepOutcome {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(api) = api {
        cmd.env("DOCKER_SANDBOXES_API", api);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => return StepOutcome::failed(action, format!("spawn: {error}")),
    };
    // Write the scenario input, then close stdin so that the command sees the
    // end of its input.
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(input.as_bytes()).await;
        drop(pipe);
    }
    // On timeout, the child is dropped and `kill_on_drop` kills it.
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => StepOutcome {
            action,
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            ..StepOutcome::default()
        },
        Ok(Err(error)) => StepOutcome::failed(action, error),
        Err(_) => StepOutcome {
            timed_out: true,
            ..StepOutcome::failed(action, "timeout")
        },
    }
}

/// The spy task of a run. Dropping it, on success or on error, stops the spy
/// and removes its socket (in swap mode, [`SwapGuard`] restores the real socket
/// instead).
struct SpyTask {
    /// The running spy.
    handle: tokio::task::JoinHandle<Result<()>>,
    /// The spy's own socket, removed on drop; `None` in swap mode.
    socket: Option<PathBuf>,
}

impl Drop for SpyTask {
    fn drop(&mut self) {
        self.handle.abort();
        if let Some(socket) = &self.socket {
            let _ = std::fs::remove_file(socket);
        }
    }
}

/// The part of the JSON `bytes` read from `file` selected by `step.pointer`,
/// wrapped under `step.wrap` when set.
fn select_json(bytes: &[u8], step: &RequestStep, file: &str) -> Result<Vec<u8>> {
    let mut value: Value =
        serde_json::from_slice(bytes).with_context(|| format!("{file} is not JSON"))?;
    if let Some(pointer) = &step.pointer {
        value = value
            .pointer(pointer)
            .cloned()
            .with_context(|| format!("{file} has nothing at {pointer}"))?;
    }
    if let Some(key) = &step.wrap {
        value = serde_json::json!({ key: value });
    }
    Ok(serde_json::to_vec(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_recognized_by_their_field() {
        let suite: Suite = serde_json::from_value(json!({
            "name": "t",
            "description": "d",
            "scenarios": [
                {"name": "a", "category": "c", "args": ["ls"]},
                {"name": "b", "category": "c", "request": {"method": "GET", "path": "/x", "hold": "k"}},
                {"name": "c", "category": "c", "release": "k"},
                {"name": "d", "category": "c", "program": "{audit}", "args": ["mcp-echo"], "tty": {"rows": 1, "cols": 2}}
            ]
        }))
        .unwrap();
        let kinds: Vec<&str> = suite
            .scenarios
            .iter()
            .map(|s| match &s.step {
                Step::Command(c) if c.program.is_some() => "program",
                Step::Command(_) => "sbx",
                Step::Request { .. } => "request",
                Step::Release { .. } => "release",
            })
            .collect();
        assert_eq!(kinds, ["sbx", "request", "release", "program"]);
    }

    #[test]
    fn verdicts_compare_the_outcome_with_the_expectation() {
        assert_eq!(verdict(true, false), Verdict::Ok);
        assert_eq!(verdict(false, true), Verdict::ExpectedError);
        assert_eq!(verdict(false, false), Verdict::Failed);
        assert_eq!(verdict(true, true), Verdict::UnexpectedSuccess);
    }

    #[test]
    fn placeholders_are_substituted_in_bodies() {
        let work = WorkDir {
            root: PathBuf::from("/tmp/w"),
        };
        let p = Placeholders::new(&work, Path::new("/tmp/w/spy.sock")).unwrap();
        let mut body = json!({"host_path": "{fixtures}/mountdir", "list": ["{work}/x"]});
        p.value(&mut body);
        assert_eq!(
            body,
            json!({"host_path": "/tmp/w/fixtures/mountdir", "list": ["/tmp/w/x"]})
        );
    }
}
