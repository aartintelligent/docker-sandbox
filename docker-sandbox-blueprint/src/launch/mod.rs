//! A sandbox launched from its spec, in seven steps, and owned by a lease.
//!
//! [`SandboxSpec::launch`] checks the spec, forges its kit in memory, draws
//! the tokens of the MCP servers of the host, prepares the keys, creates the
//! sandbox, starts it and pushes its keys, then returns the [`Lease`] that
//! holds its session (`lease.rs`), with the [`Report`] of what it did
//! (`report.rs`). Each step is the brick of a domain: the kit of `spec`, the
//! tokens of `mcp`, the keys of `key`, the rules of `network`; this module
//! orders them. Nothing is written on the host at any step; the daemon keeps
//! the image of the base kit in its cache.
//!
//! A failure once the sandbox exists removes it, unless the spec keeps it
//! for a look inside, and the error names the step. An install command that
//! fails already fails the creation, and the daemon removes the sandbox
//! itself. A launch abandoned on the way, its future dropped, cannot remove
//! anything: the sandbox bears the name the caller chose, who can remove it.

mod lease;
mod report;

pub use self::lease::Lease;
pub use self::report::Report;

use docker_sandbox::Client;
use docker_sandbox::kit::KitPackage;
use docker_sandbox::sandbox::{
    CreateRequest, Created, DeleteOptions, ProxiedKey, Sandbox, SessionGuard,
};

use docker_sandbox_kit::KitCache;

use crate::error::{Error, LaunchStep, Result};
use crate::key::Keys;
use crate::mcp::Tokens;
use crate::spec::{Kit, SandboxSpec, WorkspaceSpec};

impl SandboxSpec {
    /// Launches the sandbox `name` from the spec, its keys given `keys`, and
    /// returns the [`Lease`] that owns it.
    ///
    /// The steps: check the spec and the values of its keys
    /// ([`SandboxSpec::check`], [`SandboxSpec::check_keys`]); forge its kit
    /// in memory; draw a token for each MCP server of the host, 32 bytes of
    /// `/dev/urandom` in hexadecimal, in its variable; prepare a
    /// `ProxiedKey` for each key, its placeholder in its variable; create the
    /// sandbox with the variables, the tokens, the placeholders, the
    /// workspace, the mounts and the resources; start it, which runs its
    /// startup commands, and push its keys; return the lease, which holds
    /// its session.
    ///
    /// ```no_run
    /// # async fn run(client: docker_sandbox::Client, key: String) -> docker_sandbox_blueprint::Result<()> {
    /// use docker_sandbox_blueprint::prelude::*;
    ///
    /// let spec = SandboxSpec::agent(Agent::Claude)
    ///     .with_key(KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic"))
    ///     .with_mcp(McpServer::host("tools", 47813));
    /// let lease = spec
    ///     .launch(&client, "job-42", &Keys::new().with_key("ANTHROPIC_API_KEY", key))
    ///     .await?;
    /// // The server of the host checks this token in the requests of the agent.
    /// let token = lease.mcp_token("tools");
    /// # let _ = token;
    /// lease.shutdown().await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Before the sandbox exists, as [`SandboxSpec::check`] and
    /// [`SandboxSpec::check_keys`], and with [`Error::InvalidSpec`] when a
    /// refusal covers a host the kits declare for the service of a key;
    /// [`Error::Kit`] when the kit cannot be forged; [`Error::Client`] when
    /// the token source cannot be read or the daemon refuses the creation.
    /// Once it exists, [`Error::Launch`], which names the sandbox, the step
    /// and whether the sandbox was kept.
    pub async fn launch(
        &self,
        client: &Client,
        name: impl Into<String>,
        keys: &Keys,
    ) -> Result<Lease> {
        self.launch_from(client, name.into(), keys, None).await
    }

    /// Launches the sandbox `name` as [`SandboxSpec::launch`] does, its kit
    /// taken from `cache`: forged once per composition, then reused while it
    /// suits the daemon. The sandboxes of a job, launched from one spec,
    /// forge their kit once.
    ///
    /// ```no_run
    /// # async fn run(client: docker_sandbox::Client) -> docker_sandbox_blueprint::Result<()> {
    /// use docker_sandbox_blueprint::prelude::*;
    ///
    /// let cache = KitCache::new();
    /// let spec = SandboxSpec::agent(Agent::Shell).with_network_deny(["*.pastebin.com"]);
    /// for job in ["job-1", "job-2"] {
    ///     // Forged for the first job, taken from the cache for the second.
    ///     let lease = spec.launch_with(&client, job, &Keys::new(), &cache).await?;
    ///     lease.shutdown().await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`SandboxSpec::launch`].
    pub async fn launch_with(
        &self,
        client: &Client,
        name: impl Into<String>,
        keys: &Keys,
        cache: &KitCache,
    ) -> Result<Lease> {
        self.launch_from(client, name.into(), keys, Some(cache))
            .await
    }

    /// The launch of [`SandboxSpec::launch`], its kit taken from `cache`
    /// when there is one.
    async fn launch_from(
        &self,
        client: &Client,
        name: String,
        keys: &Keys,
        cache: Option<&KitCache>,
    ) -> Result<Lease> {
        self.check_keys(keys)?;
        let package = match (self.kit(client)?, cache) {
            (Kit::Package(package), _) => package,
            (Kit::Forge(forge), Some(cache)) => cache.package(&forge).await?,
            (Kit::Forge(forge), None) => forge.build().await?,
        };
        let tokens = self.draw_tokens()?;
        let proxied = self.proxied_keys(&package, keys)?;
        let network = self.network_report(client).await?;
        let request = self.request(&package, &name, &tokens, &proxied)?;
        let created = package.create_with(client, &request).await?;
        let sandbox = client.sandbox(&name);
        let session = match start(&sandbox, &proxied).await {
            Ok(session) => session,
            Err((step, source)) => return Err(self.failed(&sandbox, &created, step, source).await),
        };
        let report = self.report(&package, &created, &proxied, network);
        let tokens = tokens
            .into_iter()
            .map(|(server, (_, token))| (server, token))
            .collect();
        Ok(Lease::new(
            sandbox,
            session,
            created,
            package.agent(),
            report,
            tokens,
        ))
    }

    /// The creation request of the sandbox `name`.
    fn request(
        &self,
        package: &KitPackage,
        name: &str,
        tokens: &Tokens,
        proxied: &[ProxiedKey],
    ) -> Result<CreateRequest> {
        let mut request = package.request(name)?.with_envs(self.env.clone());
        for (env, token) in tokens.values() {
            request = request.with_env(env.clone(), token.expose_secret());
        }
        for key in proxied {
            request = request.with_proxied_key(key);
        }
        request = match &self.workspace {
            Some(WorkspaceSpec::Mount(dir)) => request.with_workspace(dir.clone()),
            Some(WorkspaceSpec::Clone(repository)) => {
                request.with_cloned_workspace(repository.clone())
            }
            None => request,
        };
        for mount in &self.mounts {
            request = request.with_additional_workspace(mount.clone());
        }
        if let Some(cpus) = self.resources.cpus {
            request = request.with_cpus(cpus);
        }
        if let Some(memory) = &self.resources.memory {
            request = request.with_memory(memory.clone());
        }
        Ok(request)
    }

    /// The error of a launch that failed at `step` once `sandbox` existed,
    /// which is removed unless the spec keeps it.
    async fn failed(
        &self,
        sandbox: &Sandbox,
        created: &Created,
        step: LaunchStep,
        source: docker_sandbox::Error,
    ) -> Error {
        let options = DeleteOptions::default()
            .forced()
            .with_expected_id(created.id.clone());
        let kept = self.keep_on_failure || sandbox.delete_with(&options).await.is_err();
        Error::Launch {
            sandbox: sandbox.name().to_owned(),
            step,
            kept,
            source,
        }
    }
}

/// Starts `sandbox` and pushes its keys, holding its session.
async fn start(
    sandbox: &Sandbox,
    proxied: &[ProxiedKey],
) -> std::result::Result<SessionGuard, (LaunchStep, docker_sandbox::Error)> {
    let session = sandbox
        .ensure_running()
        .await
        .map_err(|error| (LaunchStep::Start, error))?;
    if !proxied.is_empty() {
        sandbox
            .push_proxied_keys(proxied)
            .await
            .map_err(|error| (LaunchStep::Keys, error))?;
    }
    Ok(session)
}
