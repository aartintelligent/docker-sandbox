//! What a launch did, for the program and for whoever reads its logs.
//!
//! A [`Report`] says which kits make the sandbox, which hosts its rules allow
//! and refuse, where each key goes, which MCP servers its agent knows and
//! which credentials the daemon withholds. Each domain writes its own part
//! (`NetworkReport`, `KeyReport`, `ServerReport`); this module puts them
//! together. It is serializable, so that a program in another language gets
//! it as it is, and it never holds the value of a key or of a token.

use docker_sandbox::kit::KitPackage;
use docker_sandbox::sandbox::{Created, ProxiedKey};
use serde::Serialize;

use crate::key::{KeyReport, key_reports};
use crate::mcp::ServerReport;
use crate::network::NetworkReport;
use crate::spec::SandboxSpec;

/// What a launch did. Serialized as JSON, its fields keep their names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Report {
    /// The kits of the sandbox, by reference: its base kit, then its
    /// mixins, the generated ones included.
    pub kits: Vec<String>,
    /// The network rules of the spec.
    pub network: NetworkReport,
    /// Where each key goes.
    pub keys: Vec<KeyReport>,
    /// The MCP servers registered in the agent.
    pub mcp: Vec<ServerReport>,
    /// Services whose credential, declared by a kit, the daemon withholds:
    /// the proxy injects nothing for them, the subscription of the agent
    /// included.
    pub withheld: Vec<String>,
}

impl SandboxSpec {
    /// The report of a launch of the spec, from its package, its creation,
    /// its keys and its network rules.
    pub(crate) fn report(
        &self,
        package: &KitPackage,
        created: &Created,
        proxied: &[ProxiedKey],
        network: NetworkReport,
    ) -> Report {
        Report {
            kits: package.kit_refs(),
            network,
            keys: key_reports(proxied),
            mcp: self.server_reports(&package.agent().map(|agent| agent.name).unwrap_or_default()),
            withheld: created.withheld.clone(),
        }
    }
}
