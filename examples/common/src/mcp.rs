//! MCP for the examples: the tools, served on standard input and output or
//! over streamable HTTP, their registration with `sbx`, and the client that
//! checks, from inside a sandbox, what its agent would see.
//!
//! MCP servers are registered on the host with `sbx mcp add`; the daemon then
//! loads them into the gateway of a sandbox. The gateway answers inside the
//! sandbox only: its port on the host refuses requests that do not come
//! through the proxy of the sandbox. A client therefore checks a gateway from
//! inside: [`SandboxClient`] builds the `mcp-client` example as a static
//! binary for the sandbox, copies it there, and runs it.
//!
//! - **Local server**: the example registers its own binary with
//!   [`SERVE_ARG`]; started that way, it serves [`ExampleTools`] on standard
//!   input and output instead of running the example.
//! - **Remote server**: [`HttpServer`] serves the same tools over streamable
//!   HTTP on a loopback port, and only to requests carrying its bearer token,
//!   registered with a header whose `${token}` placeholder `sbx` fills from a
//!   secret it stores ([`set_secret`]).

use std::convert::Infallible;
use std::hash::{BuildHasher, RandomState};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use docker_sandbox::Client;
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::mcp::McpServerCheck;
use docker_sandbox::sandbox::Sandbox;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServiceExt, schemars, tool, tool_router};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::task::JoinHandle;

use crate::program;

/// Argument with which `sbx` starts the example binary as an MCP server.
pub const SERVE_ARG: &str = "mcp-serve";

/// Where the client is copied inside the sandbox.
const CLIENT_IN_SANDBOX: &str = "/tmp/example-mcp-client";

/// A few tools an agent can call to show that the server is reached.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExampleTools;

/// Arguments of the `sum` tool of [`ExampleTools`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SumRequest {
    /// First number.
    pub a: i64,
    /// Second number.
    pub b: i64,
}

/// Arguments of the `echo` tool of [`ExampleTools`].
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EchoRequest {
    /// Text to send back.
    pub text: String,
}

#[tool_router(server_handler)]
#[allow(
    clippy::unused_self,
    clippy::trivially_copy_pass_by_ref,
    reason = "the MCP tool macro routes calls to `&self` methods"
)]
impl ExampleTools {
    /// Adds two numbers.
    #[tool(description = "Add two numbers")]
    fn sum(&self, Parameters(SumRequest { a, b }): Parameters<SumRequest>) -> String {
        a.saturating_add(b).to_string()
    }

    /// Sends the text back, marked as coming from the host.
    #[tool(description = "Send a text back, as seen by the host")]
    fn echo(&self, Parameters(EchoRequest { text }): Parameters<EchoRequest>) -> String {
        format!("host says: {text}")
    }

    /// Current time of the host, in seconds since the Unix epoch.
    #[tool(description = "Current time of the host, in seconds since the Unix epoch")]
    fn host_time(&self) -> String {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            .to_string()
    }
}

/// Runs [`ExampleTools`] on standard input and output when the program was
/// started with [`SERVE_ARG`], and returns whether it did.
///
/// Call it first in the `main` of an example that registers itself.
///
/// # Errors
///
/// Fails when the MCP session breaks.
pub async fn serve_if_requested() -> Result<bool> {
    if std::env::args().nth(1).as_deref() != Some(SERVE_ARG) {
        return Ok(false);
    }
    ExampleTools
        .serve(rmcp::transport::stdio())
        .await
        .context("starting the MCP server")?
        .waiting()
        .await
        .context("serving MCP")?;
    Ok(true)
}

/// [`ExampleTools`] over streamable HTTP on a loopback port of the host, for
/// requests carrying `Authorization: Bearer <token>` only. The token can be
/// rotated while the server runs. Stopped when dropped.
#[derive(Debug)]
pub struct HttpServer {
    /// URL of the MCP endpoint.
    url: String,
    /// The `Authorization` header requests must carry.
    expected: Arc<RwLock<String>>,
    /// Requests that carried the token.
    authorized: Arc<AtomicUsize>,
    /// Requests refused for a missing or wrong token.
    refused: Arc<AtomicUsize>,
    /// Accept loop.
    task: JoinHandle<()>,
}

impl HttpServer {
    /// Starts the server on a free loopback port.
    ///
    /// # Errors
    ///
    /// Fails when no port can be bound.
    pub async fn start(token: &str) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding the MCP server")?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let service = Arc::new(StreamableHttpService::new(
            || Ok(ExampleTools),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        ));
        let authorized = Arc::new(AtomicUsize::new(0));
        let refused = Arc::new(AtomicUsize::new(0));
        let expected = Arc::new(RwLock::new(format!("Bearer {token}")));
        let counters = (Arc::clone(&authorized), Arc::clone(&refused));
        let shared = Arc::clone(&expected);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (service, expected) = (Arc::clone(&service), Arc::clone(&shared));
                let (authorized, refused) = (Arc::clone(&counters.0), Arc::clone(&counters.1));
                let handler = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let service = Arc::clone(&service);
                    let carries_token = request
                        .headers()
                        .get(hyper::header::AUTHORIZATION)
                        .is_some_and(|value| {
                            let expected = expected.read().unwrap_or_else(PoisonError::into_inner);
                            value.as_bytes() == expected.as_bytes()
                        });
                    let counter = if carries_token { &authorized } else { &refused };
                    counter.fetch_add(1, Ordering::Relaxed);
                    async move {
                        Ok::<_, Infallible>(if carries_token {
                            service.handle(request).await
                        } else {
                            unauthorized()
                        })
                    }
                });
                tokio::spawn(async move {
                    // A connection that breaks only ends that connection.
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), handler)
                        .await;
                });
            }
        });
        Ok(Self {
            url,
            expected,
            authorized,
            refused,
            task,
        })
    }

    /// URL of the MCP endpoint, e.g. `http://127.0.0.1:41000/mcp`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Replaces the token: requests carrying the previous one are refused
    /// from now on.
    pub fn rotate_token(&self, token: &str) {
        *self
            .expected
            .write()
            .unwrap_or_else(PoisonError::into_inner) = format!("Bearer {token}");
    }

    /// Requests that carried the token so far.
    #[must_use]
    pub fn authorized(&self) -> usize {
        self.authorized.load(Ordering::Relaxed)
    }

    /// Requests refused so far for a missing or wrong token.
    #[must_use]
    pub fn refused(&self) -> usize {
        self.refused.load(Ordering::Relaxed)
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The answer to a request without the token.
fn unauthorized() -> Response<BoxBody<Bytes, Infallible>> {
    let mut response = Response::new(Full::new(Bytes::from_static(b"unauthorized\n")).boxed());
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
}

/// An MCP server registered with `sbx mcp add`, removed by
/// [`Registration::remove`].
#[derive(Debug)]
pub struct Registration {
    /// Name of the server.
    name: String,
}

impl Registration {
    /// Checks the running binary against the MCP policy of the daemon, then
    /// registers it under `name` as a local server started with
    /// [`SERVE_ARG`].
    ///
    /// # Errors
    ///
    /// Fails when the policy refuses the server or `sbx mcp add` fails.
    pub async fn add(client: &Client, name: &str) -> Result<Self> {
        let exe = std::env::current_exe().context("locating the example binary")?;
        let exe = exe
            .to_str()
            .context("the example binary path is not UTF-8")?;
        check(client, &McpServerCheck::local(name, [exe, SERVE_ARG])).await?;
        sbx(
            &["mcp", "add", name, "--command", exe, "--args", SERVE_ARG],
            None,
        )
        .await?;
        eprintln!("registered the local MCP server {name}");
        Ok(Self {
            name: name.to_owned(),
        })
    }

    /// Checks the server at `url` against the MCP policy of the daemon, then
    /// registers it under `name` as a remote server sent `headers` (`Name:
    /// value`, where `${placeholder}` is filled from [`set_secret`]).
    ///
    /// A loopback URL needs `--skip-ssrf-check`, which is passed: the example
    /// trusts its own server.
    ///
    /// # Errors
    ///
    /// Fails when the policy refuses the server or `sbx mcp add` fails.
    pub async fn add_remote(
        client: &Client,
        name: &str,
        url: &str,
        headers: &[&str],
    ) -> Result<Self> {
        check(client, &McpServerCheck::remote(name, url)).await?;
        let mut args = vec!["mcp", "add", name, "--url", url, "--skip-ssrf-check"];
        for header in headers {
            args.extend(["--header", header]);
        }
        sbx(&args, None).await?;
        eprintln!("registered the remote MCP server {name} at {url}");
        Ok(Self {
            name: name.to_owned(),
        })
    }

    /// Name of the server, to pass to
    /// [`Sandbox::load_mcp_server`](docker_sandbox::sandbox::Sandbox::load_mcp_server).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Removes the server with `sbx mcp rm`.
    ///
    /// # Errors
    ///
    /// Fails when `sbx mcp rm` fails.
    pub async fn remove(self) -> Result<()> {
        sbx(&["mcp", "rm", &self.name, "-f"], None).await?;
        eprintln!("removed the MCP server {}", self.name);
        Ok(())
    }
}

/// A dummy bearer token, different on every call, for the HTTP server of an
/// example run: never a real credential.
#[must_use]
pub fn dummy_token() -> String {
    format!(
        "example-{:016x}",
        RandomState::new().hash_one(std::process::id())
    )
}

/// Stores `value` as the secret `mcp:<server>:<placeholder>` of `sbx`, which
/// fills `${placeholder}` in the headers of the server when the gateway
/// connects to it. The value never enters the sandbox. A gateway already
/// connected keeps the previous value: unload and load the server again for
/// it to use a new one.
///
/// # Errors
///
/// Fails when `sbx secret set` fails.
pub async fn set_secret(server: &str, placeholder: &str, value: &str) -> Result<()> {
    let key = format!("mcp:{server}:{placeholder}");
    sbx(&["secret", "set", &key, "-f"], Some(value)).await?;
    eprintln!("stored the secret {key}");
    Ok(())
}

/// Removes the secret stored by [`set_secret`], and the
/// `mcp:<server>:<placeholder>:endpoint` entry `sbx` adds the first time the
/// secret is sent, which binds it to the URL of the server and outlives the
/// secret itself.
///
/// # Errors
///
/// Fails when `sbx secret rm` fails; removing a missing secret succeeds.
pub async fn remove_secret(server: &str, placeholder: &str) -> Result<()> {
    let key = format!("mcp:{server}:{placeholder}");
    sbx(&["secret", "rm", &key, "-f"], None).await?;
    sbx(&["secret", "rm", &format!("{key}:endpoint"), "-f"], None).await?;
    eprintln!("removed the secret {key} and its endpoint binding");
    Ok(())
}

/// Checks `server` against the MCP policy of the daemon.
async fn check(client: &Client, server: &McpServerCheck) -> Result<()> {
    let check = client.mcp().check_registration(server).await?;
    if !check.allowed {
        bail!(
            "the MCP policy of the daemon refuses the server {}",
            server.name
        );
    }
    Ok(())
}

/// What an MCP client saw in one session with a server.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    /// Name the server gave in its handshake.
    pub server: String,
    /// Names of the tools offered when the session opened.
    pub tools: Vec<String>,
    /// The calls made, in order.
    pub calls: Vec<CallOutcome>,
    /// Names of the tools offered once the calls were made: a call such as
    /// the `mcp-add` tool of the gateway changes them.
    pub tools_after: Vec<String>,
}

impl Session {
    /// The tool named `name`, or whose name ends with `_<name>`: a gateway
    /// may prefix the tools of each server.
    #[must_use]
    pub fn tool(&self, name: &str) -> Option<&str> {
        let suffix = format!("_{name}");
        self.tools
            .iter()
            .map(String::as_str)
            .find(|tool| *tool == name || tool.ends_with(&suffix))
    }
}

/// The result of one tool call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallOutcome {
    /// Tool called.
    pub tool: String,
    /// Text of the result.
    pub text: String,
    /// Whether the tool reported an error.
    pub is_error: bool,
}

impl CallOutcome {
    /// The text of the result, or an error when the tool reported one.
    ///
    /// # Errors
    ///
    /// Fails when the tool reported an error.
    pub fn into_text(self) -> Result<String> {
        if self.is_error {
            bail!("the tool {} failed: {}", self.tool, self.text);
        }
        Ok(self.text)
    }
}

/// The `mcp-client` example, copied into a sandbox: an MCP client that talks
/// to the gateway exactly as the agent of the sandbox would.
#[derive(Debug, Clone)]
pub struct SandboxClient {
    /// Sandbox the client runs in.
    sandbox: Sandbox,
}

impl SandboxClient {
    /// Builds the client as a static binary for the platform of `sandbox`
    /// (`cargo build --target <arch>-unknown-linux-musl`, which needs that
    /// target: `rustup target add <arch>-unknown-linux-musl`) and copies it
    /// into the sandbox.
    ///
    /// # Errors
    ///
    /// Fails when the client cannot be built or copied.
    pub async fn install(sandbox: &Sandbox) -> Result<Self> {
        program::install(sandbox, "example-mcp-client", CLIENT_IN_SANDBOX).await?;
        Ok(Self {
            sandbox: sandbox.clone(),
        })
    }

    /// Opens a session with `url` and lists the tools.
    ///
    /// # Errors
    ///
    /// Fails when the client cannot connect.
    pub async fn list(&self, url: &str) -> Result<Session> {
        self.session(url, &[]).await
    }

    /// Opens a session with `url` and calls `tool` with `arguments`,
    /// returning the text of its result.
    ///
    /// # Errors
    ///
    /// Fails when the client cannot connect or the tool reports an error.
    pub async fn call(&self, url: &str, tool: &str, arguments: &Value) -> Result<String> {
        let session = self.session(url, &[(tool, arguments.clone())]).await?;
        session
            .calls
            .into_iter()
            .next()
            .context("the client printed no result")?
            .into_text()
    }

    /// Opens a session with `url` and makes `calls`, in order, in that
    /// session.
    ///
    /// # Errors
    ///
    /// Fails when the client cannot connect or a call breaks the session; a
    /// tool reporting an error is a [`CallOutcome`] with `is_error` set.
    pub async fn session(&self, url: &str, calls: &[(&str, Value)]) -> Result<Session> {
        let mut command = vec![CLIENT_IN_SANDBOX.to_owned(), url.to_owned()];
        for (tool, arguments) in calls {
            command.extend([(*tool).to_owned(), arguments.to_string()]);
        }
        let output = self.sandbox.exec(&ExecSpec::new(command)).await?;
        if !output.success() {
            bail!("the MCP client failed: {}", output.stderr.trim());
        }
        let lines: Vec<Value> = output
            .stdout
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Ok(session(&lines))
    }
}

/// Checks from inside `sandbox` that its gateway serves [`ExampleTools`]:
/// installs the client, then calls `sum` through the gateway, as the agent of
/// the sandbox will.
///
/// # Errors
///
/// Fails when the client cannot be installed, the gateway lists no `sum`
/// tool, or the tool answers wrongly.
pub async fn check_gateway(sandbox: &Sandbox) -> Result<()> {
    let client = SandboxClient::install(sandbox).await?;
    let url = sandbox
        .mcp_gateway()
        .await?
        .url
        .context("the gateway has no URL")?;
    let session = client.list(&url).await?;
    let sum = session
        .tool("sum")
        .context("the gateway lists no sum tool")?;
    let answer = client
        .call(&url, sum, &serde_json::json!({ "a": 2, "b": 3 }))
        .await?;
    if answer != "5" {
        bail!("the gateway answered sum(2, 3) = {answer}");
    }
    eprintln!("checked from inside the sandbox: the gateway answers sum(2, 3) = 5");
    Ok(())
}

/// The session printed by the client.
fn session(lines: &[Value]) -> Session {
    let mut session = Session::default();
    let mut listings = 0;
    for line in lines {
        if let Some(server) = line["server"].as_str() {
            server.clone_into(&mut session.server);
        }
        if let Some(tools) = line["tools"].as_array() {
            let tools = tools
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            if listings == 0 {
                session.tools = tools;
            } else {
                session.tools_after = tools;
            }
            listings += 1;
        }
        if let (Some(tool), Some(result)) = (line["tool"].as_str(), line.get("result")) {
            let text: Vec<&str> = result["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|block| block["text"].as_str())
                .collect();
            session.calls.push(CallOutcome {
                tool: tool.to_owned(),
                text: text.join("\n"),
                is_error: result["isError"].as_bool() == Some(true),
            });
        }
    }
    session
}

/// Runs the `sbx` CLI without prompts, writing `stdin` to it when given.
async fn sbx(args: &[&str], stdin: Option<&str>) -> Result<()> {
    let mut child = Command::new("sbx")
        .arg("--no-input")
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running sbx")?;
    if let (Some(value), Some(mut input)) = (stdin, child.stdin.take()) {
        input.write_all(value.as_bytes()).await?;
        input.write_all(b"\n").await?;
    }
    let output = child.wait_with_output().await.context("running sbx")?;
    if !output.status.success() {
        // The arguments may name a secret, never hold its value: they are
        // safe to show.
        bail!(
            "sbx {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
