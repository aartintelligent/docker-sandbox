//! The agents `sbx` knows, each the published kit of its name.
//!
//! The kit of an agent is the image `docker.io/sbx/<name>:latest`, as the CLI
//! resolves `sbx create <name>`: naming an agent rather than writing its
//! reference keeps a typo from reaching the daemon. Its profile
//! (`profile.rs`) says what the documentation of the agent says and neither
//! the daemon nor the kit does: the variables of its keys, and how it runs a
//! prompt without a terminal.

mod profile;

pub use self::profile::{AgentKey, Headless, HeadlessOutput};

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::Error;

/// An agent `sbx` knows, run by the published kit of its name.
///
/// Its JSON form is its name, `"claude"`, read as [`str::parse`] reads it.
///
/// ```
/// use docker_sandbox_kit::Agent;
///
/// assert_eq!(Agent::Claude.reference(), "docker.io/sbx/claude:latest");
/// assert_eq!("docker-agent".parse::<Agent>()?, Agent::DockerAgent);
/// assert_eq!(serde_json::to_value(Agent::Claude)?, "claude");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Agent {
    /// Claude Code, `claude`.
    Claude,
    /// Codex, `codex`.
    Codex,
    /// GitHub Copilot, `copilot`.
    Copilot,
    /// Cursor, `cursor`.
    Cursor,
    /// Devin, `devin`.
    Devin,
    /// Docker Agent, `docker-agent`.
    DockerAgent,
    /// Droid, `droid`.
    Droid,
    /// Kiro, `kiro`.
    Kiro,
    /// OpenCode, `opencode`.
    OpenCode,
    /// A plain shell, `shell`: no agent, the base of other kits.
    Shell,
}

impl Agent {
    /// Every agent, in the order of their names.
    pub const ALL: [Self; 10] = [
        Self::Claude,
        Self::Codex,
        Self::Copilot,
        Self::Cursor,
        Self::Devin,
        Self::DockerAgent,
        Self::Droid,
        Self::Kiro,
        Self::OpenCode,
        Self::Shell,
    ];

    /// The name of the agent, and of its kit: `claude`, `docker-agent`, …
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::Cursor => "cursor",
            Self::Devin => "devin",
            Self::DockerAgent => "docker-agent",
            Self::Droid => "droid",
            Self::Kiro => "kiro",
            Self::OpenCode => "opencode",
            Self::Shell => "shell",
        }
    }

    /// The reference of its published kit, `docker.io/sbx/<name>:latest`.
    /// The daemon may name another kit for the agent
    /// (`kit.shortcuts.<name>`), which the CLI and
    /// `KitForge::agent` (feature `forge`) follow.
    #[must_use]
    pub fn reference(self) -> String {
        format!("docker.io/sbx/{}:latest", self.name())
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Agent {
    type Err = Error;

    /// The agent named `name`, as [`Agent::name`] writes it.
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|agent| agent.name() == name)
            .ok_or_else(|| Error::InvalidKit {
                kit: name.to_owned(),
                message: "not an agent sbx knows".to_owned(),
            })
    }
}

impl Serialize for Agent {
    /// Writes the agent as its name, `claude`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Agent {
    /// Reads the agent from its name.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_are_written_and_read_as_their_name() {
        assert_eq!(
            serde_json::to_value(Agent::DockerAgent).unwrap(),
            "docker-agent"
        );
        assert_eq!(
            serde_json::from_value::<Agent>("claude".into()).unwrap(),
            Agent::Claude
        );
        let error = serde_json::from_value::<Agent>("acme".into())
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an agent sbx knows"), "{error}");
        assert!(serde_json::from_value::<Agent>(serde_json::json!(3)).is_err());
    }

    #[test]
    fn agents_name_their_published_kit_and_are_parsed_back() {
        for agent in Agent::ALL {
            assert_eq!(agent.name().parse::<Agent>().unwrap(), agent);
            assert_eq!(agent.to_string(), agent.name());
            assert_eq!(agent.reference(), format!("docker.io/sbx/{agent}:latest"));
        }
        let error = "claude-code".parse::<Agent>().unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid kit claude-code: not an agent sbx knows"
        );
    }
}
