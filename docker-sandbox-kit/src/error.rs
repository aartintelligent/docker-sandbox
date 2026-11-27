//! The error of the crate.
//!
//! A kit is forged or compiled through the client of `docker-sandbox`: what
//! goes wrong there (the socket, the daemon, a host file, an archive, a
//! package) keeps its [`docker_sandbox::Error`], wrapped in
//! [`Error::Client`]. The other variants are what only a kit can get wrong.

/// Result of the operations of this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Error of the operations of this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The client of `docker-sandbox` failed: the socket, the daemon, a host
    /// file, an archive or a kit package.
    #[error(transparent)]
    Client(#[from] docker_sandbox::Error),

    /// The `sbx` CLI could not compile a kit composition (feature
    /// `compiler`): it could not be started, rejected the composition, or
    /// sent no creation request.
    #[error("kit compilation failed: {0}")]
    Compile(String),

    /// A kit descriptor is not one this crate can forge (feature `forge`):
    /// not a v3 mixin, or declaring something the daemon is not known to
    /// accept from it.
    #[error("invalid kit {kit}: {message}")]
    InvalidKit {
        /// Name or reference of the kit.
        kit: String,
        /// What is wrong with it.
        message: String,
    },

    /// A kit package could not be forged (feature `forge`): no base kit, two
    /// kits of the same name, or a kit policy of the daemon that forbids it.
    #[error("kit forging refused: {0}")]
    Forge(String),
}

impl Error {
    /// The error of the client, if this is an [`Error::Client`].
    #[must_use]
    pub fn client(&self) -> Option<&docker_sandbox::Error> {
        match self {
            Self::Client(error) => Some(error),
            _ => None,
        }
    }

    /// Whether the daemon reported that the target does not exist, such as
    /// an image that is not present.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.client()
            .is_some_and(docker_sandbox::Error::is_not_found)
    }

    /// Whether the daemon reported a state conflict, such as an operation
    /// already in progress.
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        self.client()
            .is_some_and(docker_sandbox::Error::is_conflict)
    }

    /// Whether the daemon rejected a request as malformed or invalid.
    #[must_use]
    pub fn is_invalid_input(&self) -> bool {
        self.client()
            .is_some_and(docker_sandbox::Error::is_invalid_input)
    }

    /// Whether the operation ran out of time, the compilation by the CLI
    /// included.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        self.client().is_some_and(docker_sandbox::Error::is_timeout)
    }

    /// Whether retrying the same operation later may succeed: the daemon was
    /// unreachable, slow, busy or failing. A kit that cannot be forged or
    /// compiled is not.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        self.client()
            .is_some_and(docker_sandbox::Error::is_transient)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_of_the_client_are_kept_and_classified() {
        let error = Error::from(docker_sandbox::Error::Timeout {
            operation: "kit compilation",
            after: std::time::Duration::from_secs(1),
        });
        assert_eq!(error.to_string(), "kit compilation: no outcome within 1s");
        assert!(error.client().is_some());
        assert!(error.is_timeout() && error.is_transient());
        assert!(!error.is_not_found() && !error.is_conflict() && !error.is_invalid_input());

        let error = Error::Forge("no base kit".to_owned());
        assert_eq!(error.to_string(), "kit forging refused: no base kit");
        assert!(error.client().is_none());
        assert!(!error.is_timeout() && !error.is_transient());
        assert!(!error.is_not_found() && !error.is_conflict() && !error.is_invalid_input());
    }
}
