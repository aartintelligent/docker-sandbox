//! The error of the crate.
//!
//! A spec is checked and applied through the two other crates of the
//! repository: what goes wrong there keeps its error, the one of
//! `docker-sandbox-kit` in [`Error::Kit`] and the one of `docker-sandbox` in
//! [`Error::Client`]. A spec that breaks one of the rules of a launch is an
//! [`Error::InvalidSpec`], which names the rule.

/// Result of the operations of this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Error of the operations of this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The client of `docker-sandbox` failed: the socket or the daemon.
    #[error(transparent)]
    Client(#[from] docker_sandbox::Error),

    /// The kit of the spec cannot be forged: a mixin, an MCP server or the
    /// base kit is not one the forge takes, or the kit policy of the daemon
    /// refuses the composition.
    #[error(transparent)]
    Kit(#[from] docker_sandbox_kit::Error),

    /// The spec breaks a rule of a launch, which the message names with the
    /// field it is about.
    #[error("invalid sandbox spec: {0}")]
    InvalidSpec(String),

    /// A launch failed once its sandbox existed: the sandbox was removed,
    /// unless `kept` says it stays, kept by the spec or because it could not
    /// be removed; its `id` then removes it, and no other sandbox of its
    /// name.
    #[error("launching the sandbox {sandbox} failed {step}{}: {source}", kept_note(*.kept))]
    #[non_exhaustive]
    Launch {
        /// Name of the sandbox.
        sandbox: String,
        /// Identifier of the sandbox, which the daemon gave it at its
        /// creation.
        id: String,
        /// The step that failed.
        step: LaunchStep,
        /// Whether the sandbox stays.
        kept: bool,
        /// The error of the client, boxed to keep this error small.
        source: Box<docker_sandbox::Error>,
    },
}

/// The step of a launch that failed once its sandbox existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LaunchStep {
    /// Starting the sandbox, its startup commands included.
    Start,
    /// Pushing its keys to the proxy.
    Keys,
}

impl std::fmt::Display for LaunchStep {
    /// Writes the step as the error says it: `when starting it`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Start => "when starting it",
            Self::Keys => "when pushing its keys",
        })
    }
}

/// What the error of a launch says of the sandbox it left.
fn kept_note(kept: bool) -> &'static str {
    if kept {
        ", and the sandbox stays"
    } else {
        ", and the sandbox was removed"
    }
}

/// An [`Error::InvalidSpec`] saying `message`.
pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidSpec(message.into())
}
