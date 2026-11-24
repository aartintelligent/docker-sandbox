//! The agent a kit package runs, as its base kit declares it.
//!
//! The `kit_artifacts` the `sbx` CLI assembles stay opaque, except for the
//! manifest of the base kit: it names the program of the agent, the options
//! its sessions start it with, and the hosts it needs to reach.

use serde_json::Value;

/// The agent of a kit package, read from the manifest of its base kit.
///
/// ```
/// use docker_sandbox::kit::KitAgent;
///
/// let artifact = serde_json::json!({
///     "manifest": {
///         "name": "claude",
///         "displayName": "Claude Code",
///         "binary": "claude",
///         "runOptions": ["--dangerously-skip-permissions"]
///     },
///     "caps": { "network": { "allow": ["api.anthropic.com:443", "claude.com:443"] } }
/// });
/// let agent = KitAgent::from_artifact(&artifact).unwrap();
/// assert_eq!(
///     agent.command(["-p", "fix the tests"]),
///     ["claude", "--dangerously-skip-permissions", "-p", "fix the tests"]
/// );
/// assert_eq!(agent.hosts(), ["api.anthropic.com", "claude.com"]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KitAgent {
    /// Kit name, e.g. `claude` or `shell`.
    pub name: String,
    /// Human-readable name, e.g. `Claude Code`.
    pub display_name: Option<String>,
    /// Program of the agent, e.g. `claude`.
    pub binary: Option<String>,
    /// Options the agent sessions start the program with, e.g.
    /// `--dangerously-skip-permissions`.
    pub run_options: Vec<String>,
    /// Network destinations the kit declares, as `host:port`.
    pub network: Vec<String>,
}

impl KitAgent {
    /// The agent described by a kit artifact, or `None` when the artifact has
    /// no manifest name.
    #[must_use]
    pub fn from_artifact(artifact: &Value) -> Option<Self> {
        let manifest = artifact.get("manifest")?;
        let strings = |value: Option<&Value>| -> Vec<String> {
            value
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let text = |key: &str| manifest.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            name: text("name")?,
            display_name: text("displayName"),
            binary: text("binary"),
            run_options: strings(manifest.get("runOptions")),
            network: strings(artifact.pointer("/caps/network/allow")),
        })
    }

    /// The program of the agent, its run options, then `args`: what to run
    /// to start the agent the way its sessions do, with extra arguments.
    /// Empty when the kit declares no program.
    #[must_use]
    pub fn command(&self, args: impl IntoIterator<Item = impl Into<String>>) -> Vec<String> {
        let Some(binary) = &self.binary else {
            return Vec::new();
        };
        std::iter::once(binary.clone())
            .chain(self.run_options.iter().cloned())
            .chain(args.into_iter().map(Into::into))
            .collect()
    }

    /// Hosts of [`KitAgent::network`], without ports and duplicates, in
    /// order.
    #[must_use]
    pub fn hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = Vec::new();
        for destination in &self.network {
            let host = destination
                .rsplit_once(':')
                .map_or(destination.as_str(), |(host, _)| host);
            if !hosts.iter().any(|h| h == host) {
                hosts.push(host.to_owned());
            }
        }
        hosts
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The agent of a base kit artifact made of `manifest` and `network`.
    fn agent(manifest: &Value, network: &[&str]) -> KitAgent {
        KitAgent::from_artifact(
            &json!({ "manifest": manifest, "caps": { "network": { "allow": network } } }),
        )
        .unwrap()
    }

    #[test]
    fn the_command_is_the_program_its_run_options_then_the_arguments() {
        let claude = agent(
            &json!({ "name": "claude", "binary": "claude", "runOptions": ["--dangerously-skip-permissions"] }),
            &[],
        );
        assert_eq!(
            claude.command(["-p", "fix the tests"]),
            [
                "claude",
                "--dangerously-skip-permissions",
                "-p",
                "fix the tests"
            ]
        );
    }

    #[test]
    fn kits_without_a_program_have_no_command() {
        let shell = agent(&json!({ "name": "shell" }), &[]);
        assert!(shell.command(["--version"]).is_empty());
        assert_eq!(
            KitAgent::from_artifact(&json!({ "manifest": {} })),
            None,
            "a manifest without a name"
        );
    }

    #[test]
    fn hosts_drop_ports_and_duplicates_in_order() {
        let claude = agent(
            &json!({ "name": "claude" }),
            &[
                "api.anthropic.com:443",
                "claude.com",
                "api.anthropic.com:80",
            ],
        );
        assert_eq!(claude.hosts(), ["api.anthropic.com", "claude.com"]);
    }
}
