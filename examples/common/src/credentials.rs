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

use docker_sandbox::kit::KitPackage;
use docker_sandbox::sandbox::ProxiedKey;
use docker_sandbox_kit::AgentKey;

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

/// The keys of `keys` found in the environment, the first variable set of
/// each, which keeps its name inside the sandbox.
#[must_use]
pub fn find_in_env(keys: &[AgentKey]) -> Vec<Found> {
    keys.iter()
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

/// The keys of `keys` found in the environment, each for the hosts the
/// kits of `package` inject the key of its service into
/// (`ProxiedKey::for_service`). A key whose service no kit declares is left
/// out, with the reason: it would have nowhere to go.
#[must_use]
pub fn proxied_keys(package: &KitPackage, keys: &[AgentKey]) -> Vec<ProxiedKey> {
    let mut proxied = Vec::new();
    for found in find_in_env(keys) {
        match ProxiedKey::for_service(package, found.service, found.env, found.value) {
            Ok(key) => proxied.push(key),
            Err(error) => eprintln!("{} is left out: {error}", found.env),
        }
    }
    proxied
}

/// A line naming `keys` and their hosts, for the terminal.
#[must_use]
pub fn describe(keys: &[ProxiedKey]) -> String {
    keys.iter()
        .map(|key| format!("{} for {}", key.env(), key.hosts().join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}
