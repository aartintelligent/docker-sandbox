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
}

/// An [`Error::InvalidSpec`] saying `message`.
pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidSpec(message.into())
}
