//! Network policy: the rules deciding what sandboxes may reach, checks of a
//! target against them, and the activity log of the proxy.
//!
//! Obtained with [`Client::policy`](crate::Client::policy). The rules of one
//! sandbox are also reachable from its handle: [`Sandbox::allow_hosts`],
//! [`Sandbox::deny_hosts`], [`Sandbox::remove_hosts`] and
//! [`Sandbox::network_rules`]. The global setup of the policy
//! (`POST /policy/network/setup`) is deliberately not exposed.

mod activity;
mod rules;

use serde::Deserialize;
use serde_json::Value;

pub use self::activity::{CheckKind, CheckResult, HostActivity, NetworkCheck, NetworkLog};
use self::rules::RuleChangeResponse;
pub use self::rules::{
    CreatedRule, Decision, HttpTarget, NetworkRule, NetworkRules, Protocol, RuleAction, RuleChange,
    RuleChangeResult,
};
use crate::client::Client;
use crate::error::{Error, Result};
use crate::sandbox::Sandbox;
use crate::transport::Request;

/// Network policy operations. Obtained with [`Client::policy`].
#[derive(Debug, Clone)]
pub struct Policy {
    /// Client the requests go through.
    client: Client,
}

impl Policy {
    /// Policy operations through `client`.
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Every rule in force, global and sandbox-scoped
    /// (`GET /policy/network/rules?type=all`).
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn rules(&self) -> Result<NetworkRules> {
        let request = Request::get("network rules", "/policy/network/rules").query("type", "all");
        self.client.transport().json(request).await
    }

    /// Applies rule changes (`POST /policy/network/rules`) and returns one
    /// outcome per change, in order.
    ///
    /// The daemon answers HTTP 200 even when a change fails, for instance an
    /// unknown action or rule identifier: each outcome carries its own
    /// [`RuleChangeResult::error`]. [`Policy::apply_all`] fails on the first
    /// one instead.
    ///
    /// # Errors
    ///
    /// Fails with an invalid-input [`Error::Api`] for an empty list.
    pub async fn apply(&self, changes: &[RuleChange]) -> Result<Vec<RuleChangeResult>> {
        /// Body of the request.
        #[derive(serde::Serialize)]
        struct Body<'a> {
            /// The changes.
            actions: &'a [RuleChange],
        }
        let request = Request::post("network rule change", "/policy/network/rules")
            .json(&Body { actions: changes })?;
        let response: RuleChangeResponse = self.client.transport().json(request).await?;
        Ok(response.results)
    }

    /// Applies rule changes and fails if one of them failed.
    ///
    /// # Errors
    ///
    /// See [`Policy::apply`]; returns [`Error::Failed`] with the reason of the
    /// first failed change.
    pub async fn apply_all(&self, changes: &[RuleChange]) -> Result<Vec<RuleChangeResult>> {
        self.apply(changes)
            .await?
            .into_iter()
            .map(RuleChangeResult::into_result)
            .collect()
    }

    /// Removes the rule of the global policy whose identifier is `id`. A rule
    /// scoped to a sandbox is removed with
    /// [`Sandbox::remove_network_rule`]: the daemon only looks for it there.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Failed`] when the global policy has no rule with that
    /// identifier, a sandbox-scoped one included.
    pub async fn remove_rule(&self, id: &str) -> Result<()> {
        self.apply_all(&[RuleChange::remove_rule(id)])
            .await
            .map(drop)
    }

    /// Checks a target against the policy (`POST /policy/network/check`).
    ///
    /// # Errors
    ///
    /// Fails with an invalid-input [`Error::Api`] if the check has no target
    /// or an unknown kind.
    pub async fn check(&self, check: &NetworkCheck) -> Result<CheckResult> {
        let request = Request::post("network check", "/policy/network/check").json(check)?;
        self.client.transport().json(request).await
    }

    /// Hosts the sandboxes reached or were refused (`GET /policy/network/log`).
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn log(&self) -> Result<NetworkLog> {
        let request = Request::get("network log", "/policy/network/log");
        self.client.transport().json(request).await
    }

    /// Policy profiles (`GET /policy/network/profiles`). Their shape has not
    /// been observed yet, so they are returned as JSON values.
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn profiles(&self) -> Result<Vec<Value>> {
        /// Response of the route.
        #[derive(Deserialize)]
        struct Profiles {
            /// The profiles.
            #[serde(default)]
            profiles: Vec<Value>,
        }
        let request = Request::get("network profiles", "/policy/network/profiles");
        let response: Profiles = self.client.transport().json(request).await?;
        Ok(response.profiles)
    }

    /// Whether the network policy still has to be set up
    /// (`GET /policy/network/setup`). The setup itself is left to the `sbx`
    /// CLI.
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn setup_needed(&self) -> Result<bool> {
        /// Response of the route.
        #[derive(Deserialize)]
        struct Setup {
            /// Whether the setup is needed.
            needed: bool,
        }
        let request = Request::get("network policy setup", "/policy/network/setup");
        let response: Setup = self.client.transport().json(request).await?;
        Ok(response.needed)
    }

    /// Reloads the policy (`POST /policy/refresh`).
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn refresh(&self) -> Result<()> {
        let request = Request::post("policy refresh", "/policy/refresh");
        self.client.transport().unit(request).await
    }
}

impl Sandbox {
    /// Rules scoped to this sandbox.
    ///
    /// # Errors
    ///
    /// Fails if the daemon is unreachable or answers with an error.
    pub async fn network_rules(&self) -> Result<Vec<NetworkRule>> {
        let rules = self.client().policy().rules().await?;
        Ok(rules
            .rules
            .into_iter()
            .filter(|rule| rule.is_for_sandbox(self.name()))
            .collect())
    }

    /// Allows TCP connections from this sandbox to `hosts`.
    ///
    /// ```no_run
    /// # async fn run(sandbox: docker_sandbox::sandbox::Sandbox) -> docker_sandbox::Result<()> {
    /// sandbox.allow_hosts(["api.github.com", "*.githubusercontent.com"]).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Failed`] when the daemon refuses the change.
    pub async fn allow_hosts(
        &self,
        hosts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<RuleChangeResult> {
        self.apply_rule(RuleChange::allow(hosts)).await
    }

    /// Denies TCP and UDP connections from this sandbox to `hosts`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Failed`] when the daemon refuses the change.
    pub async fn deny_hosts(
        &self,
        hosts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<RuleChangeResult> {
        self.apply_rule(RuleChange::deny(hosts)).await
    }

    /// Removes the rules added for `hosts` in this sandbox.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Failed`] when the daemon refuses the change.
    pub async fn remove_hosts(
        &self,
        hosts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<RuleChangeResult> {
        self.apply_rule(RuleChange::remove(hosts)).await
    }

    /// Removes the rule of this sandbox whose identifier is `id`, as listed by
    /// [`Sandbox::network_rules`] or returned when the rule was created.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Failed`] when the sandbox has no rule with that
    /// identifier.
    pub async fn remove_network_rule(&self, id: &str) -> Result<RuleChangeResult> {
        self.apply_rule(RuleChange::remove_rule(id)).await
    }

    /// Applies `change`, scoped to this sandbox, and fails if the daemon
    /// reports it failed.
    async fn apply_rule(&self, change: RuleChange) -> Result<RuleChangeResult> {
        let change = change.for_sandbox(self.name());
        self.client()
            .policy()
            .apply(std::slice::from_ref(&change))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::Failed {
                operation: "network rule change",
                message: "the daemon returned no outcome".to_owned(),
            })?
            .into_result()
    }
}
