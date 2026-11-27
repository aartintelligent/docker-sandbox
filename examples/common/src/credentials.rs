//! Agent keys read from the environment and given to one sandbox through its
//! proxy.
//!
//! The socket never reads the secrets stored with `sbx secret`: the daemon
//! applies them itself. The credential a kit declares for its agent
//! (`anthropic` for `claude`, …) gets the value the daemon holds for its
//! service, a subscription for instance, when the daemon authorizes it; the
//! creation of the sandbox prints those it withholds. A key found in the
//! environment goes as a `ProxiedKey` instead: the sandbox is created with a
//! placeholder in the variable the agent reads, and the proxy of the sandbox
//! writes the key in its place in the requests to the hosts the kit declares
//! for the service of the key. Nothing is stored, the key never enters the
//! sandbox, and it goes away with the sandbox.

use anyhow::Result;
use docker_sandbox::kit::KitPackage;
use docker_sandbox::sandbox::ProxiedKey;
use serde_json::Value;

/// A credential an agent can use: an `sbx` service and the environment
/// variables it is read from, first match wins.
#[derive(Debug, Clone, Copy)]
pub struct Credential {
    /// Service known to `sbx`, as listed by `sbx secret set --help`.
    pub service: &'static str,
    /// Environment variables holding the value, in order of preference. The
    /// one found keeps its name inside the sandbox.
    pub env: &'static [&'static str],
}

/// A key found in the environment.
#[derive(Debug)]
pub struct Found {
    /// Service of the key.
    pub service: &'static str,
    /// Variable it was read from.
    pub env: &'static str,
    /// The key.
    value: String,
}

/// The credentials of `credentials` found in the environment.
#[must_use]
pub fn find_in_env(credentials: &[Credential]) -> Vec<Found> {
    credentials
        .iter()
        .filter_map(|credential| {
            credential.env.iter().find_map(|&env| {
                std::env::var(env)
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(|value| Found {
                        service: credential.service,
                        env,
                        value,
                    })
            })
        })
        .collect()
}

/// The keys of `credentials` found in the environment, each for the hosts the
/// kits of `package` inject the key of its service into. A key whose service
/// no kit declares is left out, with a warning: it would have nowhere to go.
///
/// # Errors
///
/// Fails when a key cannot be made, for a variable that is not a valid name.
pub fn proxied_keys(package: &KitPackage, credentials: &[Credential]) -> Result<Vec<ProxiedKey>> {
    let mut keys = Vec::new();
    for found in find_in_env(credentials) {
        let hosts = declared_hosts(package, found.service);
        if hosts.is_empty() {
            eprintln!(
                "{} is left out: no kit of the sandbox declares where the {} key goes",
                found.env, found.service
            );
            continue;
        }
        keys.push(ProxiedKey::new(found.env, hosts, found.value)?);
    }
    Ok(keys)
}

/// A line naming `keys` and their hosts, for the terminal.
#[must_use]
pub fn describe(keys: &[ProxiedKey]) -> String {
    keys.iter()
        .map(|key| format!("{} for {}", key.env(), key.hosts().join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The hosts the kits of `package` inject the API key of `service` into, as
/// their credentials declare them (`apiKey.inject[].domain`).
fn declared_hosts(package: &KitPackage, service: &str) -> Vec<String> {
    let mut hosts: Vec<String> = package
        .kit_artifacts()
        .iter()
        .flat_map(|artifact| list(&artifact["credentials"]))
        .filter(|credential| credential["service"] == service)
        .flat_map(|credential| list(&credential["apiKey"]["inject"]))
        .filter_map(|inject| inject["domain"].as_str().map(str::to_owned))
        .collect();
    hosts.dedup();
    hosts
}

/// The items of `value`, a JSON array, or none.
fn list(value: &Value) -> Vec<Value> {
    value.as_array().cloned().unwrap_or_default()
}
