//! The agents `sbx` knows, each the published kit of its name.
//!
//! The kit of an agent is the image `docker.io/sbx/<name>:latest`, as the CLI
//! resolves `sbx create <name>`: naming an agent rather than writing its
//! reference keeps a typo from reaching the daemon.

use std::fmt;
use std::str::FromStr;

use crate::error::Error;

/// An agent `sbx` knows, run by the published kit of its name.
///
/// ```
/// use docker_sandbox_kit::Agent;
///
/// assert_eq!(Agent::Claude.reference(), "docker.io/sbx/claude:latest");
/// assert_eq!("docker-agent".parse::<Agent>()?, Agent::DockerAgent);
/// # Ok::<(), docker_sandbox_kit::Error>(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
