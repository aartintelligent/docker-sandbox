//! Agent credentials read from the environment and given to one sandbox.
//!
//! The socket never reads the secrets stored with `sbx secret`: a program
//! pushes the credentials of a sandbox itself. The examples read them from
//! environment variables and push them as sandbox-scoped service secrets,
//! which the proxy of the sandbox injects on outgoing requests. Nothing is
//! stored, and the secret goes away with the sandbox.

use anyhow::Result;
use docker_sandbox::sandbox::Sandbox;

/// A credential an agent can use: an `sbx` service and the environment
/// variables it is read from, first match wins.
#[derive(Debug, Clone, Copy)]
pub struct Credential {
    /// Service known to `sbx`, as listed by `sbx secret set --help`.
    pub service: &'static str,
    /// Environment variables holding the value, in order of preference.
    pub env: &'static [&'static str],
}

/// The credentials of `credentials` found in the environment, as
/// `(service, value)` pairs.
#[must_use]
pub fn find_in_env(credentials: &[Credential]) -> Vec<(&'static str, String)> {
    credentials
        .iter()
        .filter_map(|credential| {
            credential
                .env
                .iter()
                .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
                .map(|value| (credential.service, value))
        })
        .collect()
}

/// Pushes to `sandbox` every credential of `credentials` found in the
/// environment, and returns the services pushed.
///
/// # Errors
///
/// Fails when the daemon refuses an update.
pub async fn push_from_env(
    sandbox: &Sandbox,
    credentials: &[Credential],
) -> Result<Vec<&'static str>> {
    let mut pushed = Vec::new();
    for (service, value) in find_in_env(credentials) {
        sandbox.push_service_secret(service, value).await?;
        pushed.push(service);
    }
    Ok(pushed)
}
