//! The network rules of a spec: the hosts it allows and refuses, as a mixin
//! of the kit.
//!
//! The rules become a generated `network-policy@1` mixin, so that the daemon
//! applies them from the creation of the sandbox, its install commands
//! included, and removes them with it. Every host is a [`HostPattern`]: the
//! daemon stores `https://host` or `host/path` and never applies them. A
//! refusal wins over any allowance (the `network` suite of the audit), so a
//! refusal that covers the host of a key or of an MCP server of the spec is
//! refused before the sandbox is created.

mod host;

pub(crate) use self::host::{authority, covers};

use docker_sandbox::policy::HostPattern;
use docker_sandbox_kit::{KitDescriptor, Mixin};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, invalid};
use crate::spec::SandboxSpec;

/// Reference of the mixin of the network rules of a spec, beside the
/// `/kits/docker-sandbox-mcp-<agent>` of the MCP servers.
pub(crate) const NETWORK_MIXIN: &str = "/kits/docker-sandbox-network";

/// The network rules of a spec, as host patterns.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Network {
    /// Hosts allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) allow: Vec<String>,
    /// Hosts refused, whatever else allows them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) deny: Vec<String>,
}

impl Network {
    /// Whether the spec names no rule.
    pub(crate) fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }
}

impl SandboxSpec {
    /// Allows `hosts`, host patterns as `HostPattern` reads them. A refusal
    /// wins over an allowance, and the global policy of the daemon allows
    /// every host by default: an allowance restricts nothing.
    #[must_use]
    pub fn with_network_allow(
        mut self,
        hosts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.network.allow.extend(hosts.into_iter().map(Into::into));
        self
    }

    /// Refuses `hosts`, host patterns as `HostPattern` reads them, whatever
    /// else allows them.
    #[must_use]
    pub fn with_network_deny(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.network.deny.extend(hosts.into_iter().map(Into::into));
        self
    }

    /// The mixin of the network rules of the spec, if it names any. Its
    /// rules apply from the creation of the sandbox, install commands
    /// included, and go with it.
    pub(crate) fn network_mixin(&self) -> Result<Option<Mixin>> {
        if self.network.is_empty() {
            return Ok(None);
        }
        let mut descriptor = KitDescriptor::mixin("Network rules")
            .with_description("The network rules of a sandbox spec");
        if !self.network.allow.is_empty() {
            descriptor = descriptor.with_network_allow(self.network.allow.clone());
        }
        if !self.network.deny.is_empty() {
            descriptor = descriptor.with_network_deny(self.network.deny.clone());
        }
        Mixin::new(NETWORK_MIXIN, descriptor)
            .map(Some)
            .map_err(Error::from)
    }

    /// The refusals of `network`, every host of it read as a host pattern.
    pub(crate) fn refusals(&self) -> Result<Vec<HostPattern>> {
        for host in &self.network.allow {
            HostPattern::parse(host.as_str())
                .map_err(|error| invalid(format!("network.allow: {error}")))?;
        }
        self.network
            .deny
            .iter()
            .map(|host| {
                HostPattern::parse(host.as_str())
                    .map_err(|error| invalid(format!("network.deny: {error}")))
            })
            .collect()
    }

    /// Checks that no refusal covers a host a key of the spec goes to or an
    /// MCP server is reached at.
    pub(crate) fn check_reach(&self) -> Result<()> {
        let deny = self.refusals()?;
        let mut reached: Vec<(String, u16, String)> = Vec::new();
        for key in &self.keys {
            for host in key.hosts() {
                let (host, port) = authority(host, 443);
                reached.push((host.to_owned(), port, format!("the key {}", key.env())));
            }
        }
        for server in &self.mcp {
            if let Some(url) = server.url() {
                let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
                let default = if scheme == "http" { 80 } else { 443 };
                let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
                let (host, port) = authority(host, default);
                reached.push((
                    host.to_owned(),
                    port,
                    format!("the MCP server {}", server.name()),
                ));
            }
        }
        for pattern in &deny {
            if let Some((host, port, what)) = reached
                .iter()
                .find(|(host, port, _)| covers(pattern, host, *port))
            {
                return Err(invalid(format!(
                    "network.deny: {pattern} refuses {host}:{port}, where {what} goes; a \
                     refusal wins over any allowance"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use docker_sandbox_kit::{Agent, McpServer};

    use super::*;
    use crate::key::KeySpec;
    use crate::spec::broken;

    /// A claude spec.
    fn claude() -> SandboxSpec {
        SandboxSpec::agent(Agent::Claude)
    }

    #[test]
    fn refusals_never_cover_a_host_the_sandbox_needs() {
        let key = claude()
            .with_key(KeySpec::for_hosts("ACME_TOKEN", ["api.acme.example"]))
            .with_network_deny(["*.acme.example"]);
        assert_eq!(
            broken(&key),
            "network.deny: *.acme.example refuses api.acme.example:443, where the key ACME_TOKEN \
             goes; a refusal wins over any allowance"
        );
        let host = claude()
            .with_mcp(McpServer::host("tools", 47813))
            .with_network_deny(["host.docker.internal:47813"]);
        assert!(broken(&host).contains("where the MCP server tools goes"));
        let remote = claude()
            .with_mcp(McpServer::remote("docs", "http://mcp.example.com/mcp"))
            .with_network_deny(["mcp.example.com:80"]);
        assert!(broken(&remote).contains("refuses mcp.example.com:80"));
        // Another port, or another host, is not covered.
        claude()
            .with_mcp(McpServer::remote(
                "docs",
                "https://mcp.example.com:8443/mcp",
            ))
            .with_mcp(McpServer::stdio("files", "mcp-fs"))
            .with_network_deny(["mcp.example.com:443", "*.pastebin.com"])
            .with_network_allow(["**"])
            .prepare()
            .unwrap();
        let allow = claude().with_network_allow(["api.example.com/v1"]);
        assert!(broken(&allow).starts_with("network.allow: invalid host pattern"));
        assert!(broken(&claude().with_network_deny(["*"])).starts_with("network.deny: "));
    }

    #[test]
    fn the_network_rules_are_a_mixin() {
        assert!(claude().network_mixin().unwrap().is_none());
        let mixin = claude()
            .with_network_allow(["api.example.com"])
            .network_mixin()
            .unwrap()
            .unwrap();
        assert_eq!(mixin.reference(), NETWORK_MIXIN);
        let deny = claude()
            .with_network_deny(["*.pastebin.com"])
            .network_mixin()
            .unwrap()
            .unwrap();
        assert_eq!(deny.name(), "docker-sandbox-network");
    }
}
