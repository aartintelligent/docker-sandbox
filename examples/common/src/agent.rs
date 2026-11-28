//! Running a coding agent in a throw-away sandbox, with or without an MCP
//! server.
//!
//! [`run`] is the whole flow of the `agent-*` examples: forge the kit of the
//! agent without the `sbx` CLI ([`kit::forged`]), create a sandbox from it
//! with the placeholders of the keys found in the environment, give the keys
//! to its proxy ([`credentials`]), load the MCP server of the `-mcp` variant
//! and check it from inside the sandbox, and plug the terminal into the agent
//! session, opened with what `sbx run` passes from the host ([`host`]). Without a key, the agent uses
//! the credential the daemon holds for its kit, such as a subscription,
//! unless the daemon withholds it (the creation says so); then it offers its
//! own login inside the session, when it has one.

use anyhow::{Context, Result, bail};

use crate::credentials;
use crate::{connect, host, kit, mcp, sandbox, terminal};

/// A coding agent known to `sbx create`, with its profile: the keys it takes
/// and how it runs a prompt without a terminal (`docker-sandbox-kit`).
pub use docker_sandbox_kit::Agent;

/// Claude Code, as `sbx create claude` names it.
pub const CLAUDE: Agent = Agent::Claude;

/// Codex, as `sbx create codex` names it.
pub const CODEX: Agent = Agent::Codex;

/// GitHub Copilot, as `sbx create copilot` names it.
pub const COPILOT: Agent = Agent::Copilot;

/// Cursor, as `sbx create cursor` names it.
pub const CURSOR: Agent = Agent::Cursor;

/// Devin, as `sbx create devin` names it.
pub const DEVIN: Agent = Agent::Devin;

/// Docker Agent, as `sbx create docker-agent` names it.
pub const DOCKER_AGENT: Agent = Agent::DockerAgent;

/// Droid, as `sbx create droid` names it.
pub const DROID: Agent = Agent::Droid;

/// Kiro, as `sbx create kiro` names it.
pub const KIRO: Agent = Agent::Kiro;

/// OpenCode, as `sbx create opencode` names it.
pub const OPENCODE: Agent = Agent::OpenCode;

/// The variables of [`Agent::forwarded_env`] set in the environment, as
/// `(name, value)` pairs.
#[must_use]
pub fn forwarded_values(agent: Agent) -> Vec<(String, String)> {
    agent
        .forwarded_env()
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| ((*name).to_owned(), value))
        })
        .collect()
}

/// Runs `agent` in a throw-away sandbox, with the MCP server of the examples
/// when `with_mcp` is set, and returns once the agent session ends.
///
/// Call it from `main`: when `sbx` starts the binary as the MCP server, it
/// serves MCP instead.
///
/// # Errors
///
/// Fails when the kit cannot be forged, the sandbox cannot be created or
/// started, or the session breaks. The sandbox and the MCP registration are
/// removed in every case.
pub async fn run(agent: &Agent, with_mcp: bool) -> Result<()> {
    if mcp::serve_if_requested().await? {
        return Ok(());
    }
    let launch = Launch::parse(std::env::args().skip(1))?;
    let client = connect().await?;
    let package = kit::forged(&client, agent.name()).await?;
    let name = sandbox::unique_name(agent.name());
    let registration = if with_mcp {
        Some(mcp::Registration::add(&client, &format!("{name}-tools")).await?)
    } else {
        None
    };
    let server = registration.as_ref().map(|r| r.name().to_owned());
    // What `sbx run` adds from the host: the SSH agent of the sandbox, and
    // the terminal, time zone and desktop session of the agent session.
    let keys = credentials::proxied_keys(&package, agent.keys());
    let mut request = package.request(&name)?;
    for key in &keys {
        request = request.with_proxied_key(key);
    }
    if let Some(socket) = host::ssh_agent_socket(&client).await {
        request = request.with_ssh_agent_socket(socket);
    }
    if let Some(dir) = &launch.workspace {
        eprintln!("workspace {dir}: mounted read-write, the agent changes its files");
        request = request.with_workspace(dir);
    }
    let session = host::agent_session(&client, terminal::size())
        .await
        .with_envs(forwarded_values(*agent))
        .with_args(launch.args);

    let outcome = sandbox::with_sandbox_from(&client, &package, &request, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        if keys.is_empty() {
            eprintln!(
                "no key in the environment: the agent uses the credential the daemon holds for \
                 its kit, or offers its own login"
            );
        } else {
            sandbox.push_proxied_keys(&keys).await?;
            eprintln!(
                "keys given to the proxy of the sandbox, which holds their placeholders: {}",
                credentials::describe(&keys)
            );
        }
        if let Some(server) = &server {
            sandbox.load_mcp_server(server).await?;
            mcp::check_gateway(&sandbox).await?;
            eprintln!("MCP server {server} loaded: ask the agent to call its `sum` tool");
        }
        let code = terminal::relay(sandbox.agent_session(&session).await?).await?;
        eprintln!("agent exited with code {code}");
        Ok(())
    })
    .await;

    if let Some(registration) = registration {
        registration.remove().await?;
    }
    outcome
}

/// What follows the name of an agent example on its command line, as for
/// `sbx run AGENT [PATH] [-- AGENT_ARGS...]`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Launch {
    /// Host directory the agent works on, mounted read-write at its own path.
    /// Without one, the agent works in the filesystem of the sandbox.
    workspace: Option<String>,
    /// Arguments of the agent: options added to its default command, or a
    /// prompt in its place.
    args: Vec<String>,
}

impl Launch {
    /// Reads `[PATH] [-- AGENT_ARGS...]` from `words`.
    ///
    /// # Errors
    ///
    /// Fails when the workspace does not exist, or when more than one path
    /// comes before `--`.
    fn parse(words: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut words = words.into_iter();
        let mut launch = Self::default();
        while let Some(word) = words.next() {
            if word == "--" {
                launch.args = words.collect();
                break;
            }
            if launch.workspace.is_some() {
                bail!("one workspace at most, then `--` and the arguments of the agent");
            }
            let dir = std::fs::canonicalize(&word).with_context(|| format!("workspace {word}"))?;
            launch.workspace = Some(dir.display().to_string());
        }
        Ok(launch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `words` as owned strings.
    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn a_workspace_and_agent_arguments_are_read_as_sbx_run_reads_them() {
        let dir = std::env::temp_dir();
        let path = dir.display().to_string();
        let canonical = std::fs::canonicalize(&dir).unwrap().display().to_string();

        assert_eq!(Launch::parse(words(&[])).unwrap(), Launch::default());
        let launch = Launch::parse(words(&[&path, "--", "-p", "fix the build"])).unwrap();
        assert_eq!(launch.workspace.as_deref(), Some(canonical.as_str()));
        assert_eq!(launch.args, ["-p", "fix the build"]);
        let launch = Launch::parse(words(&["--", "--continue"])).unwrap();
        assert_eq!(launch.workspace, None);
        assert_eq!(launch.args, ["--continue"]);

        assert!(Launch::parse(words(&["/zz-docker-sandbox-examples-none"])).is_err());
        assert!(Launch::parse(words(&[&path, &path])).is_err());
    }
}
