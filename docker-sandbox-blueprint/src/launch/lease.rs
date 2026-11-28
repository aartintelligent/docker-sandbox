//! The handle a launch returns: it owns the sandbox and its session.
//!
//! A [`Lease`] keeps the sandbox running while the program works in it, with
//! the whole API of `docker-sandbox`, then removes it with
//! [`Lease::shutdown`]. Dropped without it, the lease closes its session: the
//! sandbox stops about 30 s later and stays, named in a warning of `tracing`.
//! No removal runs in a hidden task, as a drop cannot wait for one.

use std::collections::BTreeMap;

use docker_sandbox::kit::KitAgent;
use docker_sandbox::sandbox::{Created, DeleteOptions, Sandbox, Secret, SessionGuard};

use super::report::Report;
use crate::error::Result;

/// A sandbox launched from a spec, running while the lease holds its
/// session.
///
/// ```no_run
/// # async fn run(client: docker_sandbox::Client) -> docker_sandbox_blueprint::Result<()> {
/// use docker_sandbox_blueprint::prelude::*;
///
/// let lease = SandboxSpec::agent(Agent::Shell)
///     .launch(&client, "job-42", &Keys::new())
///     .await?;
/// let output = lease.sandbox().exec(&ExecSpec::shell("uname -a")).await?;
/// println!("{}", output.stdout);
/// lease.shutdown().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Lease {
    /// The sandbox.
    sandbox: Sandbox,
    /// The session that keeps it running.
    session: SessionGuard,
    /// The answer of its creation.
    created: Created,
    /// The agent its base kit declares.
    agent: Option<KitAgent>,
    /// What the launch did.
    report: Report,
    /// The tokens of the MCP servers of the host, by server.
    tokens: BTreeMap<String, Secret>,
    /// Warns when the lease is dropped without being shut down or detached.
    warning: DropWarning,
}

/// Warns, when dropped armed, that a lease was dropped without
/// [`Lease::shutdown`] or [`Lease::detach`].
#[derive(Debug)]
struct DropWarning {
    /// The sandbox left, while armed.
    sandbox: Option<String>,
}

impl Lease {
    /// The lease of `sandbox`, held by `session`.
    pub(crate) fn new(
        sandbox: Sandbox,
        session: SessionGuard,
        created: Created,
        agent: Option<KitAgent>,
        report: Report,
        tokens: BTreeMap<String, Secret>,
    ) -> Self {
        let warning = DropWarning {
            sandbox: Some(sandbox.name().to_owned()),
        };
        Self {
            sandbox,
            session,
            created,
            agent,
            report,
            tokens,
            warning,
        }
    }

    /// The sandbox: run commands in it, read and write its files, publish a
    /// port.
    #[must_use]
    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// The answer of its creation, [`Created::withheld`] included.
    #[must_use]
    pub fn created(&self) -> &Created {
        &self.created
    }

    /// The agent the base kit declares, its program and the options its
    /// sessions start it with: what a task runs, unattended, with its own
    /// arguments (`agent.command(["-p", prompt])` for claude). `None` for a
    /// kit that declares no agent.
    #[must_use]
    pub fn agent(&self) -> Option<&KitAgent> {
        self.agent.as_ref()
    }

    /// What the launch did.
    #[must_use]
    pub fn report(&self) -> &Report {
        &self.report
    }

    /// The token of the MCP server of the host named `server`, which the
    /// program checks in the `Authorization` header of the requests the
    /// agent sends it. Drawn at launch, it is the sandbox's own.
    #[must_use]
    pub fn mcp_token(&self, server: &str) -> Option<&Secret> {
        self.tokens.get(server)
    }

    /// Closes the session and removes the sandbox, its network rules and its
    /// keys with it. The sandbox removed is the one the launch created,
    /// never another one given the same name since.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Client`](crate::Error::Client) when the daemon cannot
    /// remove the sandbox, which then stays.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            sandbox,
            session,
            created,
            mut warning,
            ..
        } = self;
        warning.disarm();
        session.release();
        let options = DeleteOptions::default()
            .forced()
            .with_expected_id(created.id);
        sandbox.delete_with(&options).await?;
        Ok(())
    }

    /// Gives the sandbox and its session to the program, which keeps the
    /// sandbox beyond the lease: it runs while the session is held, and the
    /// program removes it.
    pub fn detach(self) -> (Sandbox, SessionGuard) {
        let Self {
            sandbox,
            session,
            mut warning,
            ..
        } = self;
        warning.disarm();
        (sandbox, session)
    }
}

impl DropWarning {
    /// Keeps the warning from being given.
    fn disarm(&mut self) {
        self.sandbox = None;
    }
}

impl Drop for DropWarning {
    /// Warns that the sandbox of a dropped lease stays.
    fn drop(&mut self) {
        if let Some(sandbox) = &self.sandbox {
            tracing::warn!(
                sandbox,
                "a lease was dropped without shutdown: its session is closed, and the sandbox \
                 stops about 30 s later and stays until it is removed"
            );
        }
    }
}
