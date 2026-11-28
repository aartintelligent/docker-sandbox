//! The API keys of a sandbox: where each goes, written in the spec, and
//! their values, given at launch.
//!
//! A [`KeySpec`] names the variable an agent reads its key from and the
//! hosts the proxy writes the key for: those the kits declare for a service
//! (`anthropic` for the claude kit), or hosts written out. It becomes a
//! [`ProxiedKey`](docker_sandbox::sandbox::ProxiedKey): the sandbox holds a
//! placeholder, and the key itself only travels over the socket. The spec
//! never holds the value: [`Keys`] carries it to the launch, so that one spec
//! serves every sandbox of a job and holds nothing secret.

use std::collections::BTreeMap;

use docker_sandbox::kit::KitPackage;
use docker_sandbox::policy::HostPattern;
use docker_sandbox::sandbox::{ProxiedKey, Secret};
use serde::{Deserialize, Serialize};

use crate::error::{Result, invalid};
use crate::network::covers;
use crate::spec::SandboxSpec;

/// Where an API key goes: the variable the agent reads it from, and the hosts
/// the proxy writes it for.
///
/// Its JSON form is `{"env", "service"}` or `{"env", "hosts"}`, with
/// `"optional": true` for an optional key.
///
/// An optional key ([`KeySpec::optional`]) may be given no value at launch,
/// and a key of a service no kit of the sandbox declares is then left out
/// rather than refused: a program that hands an agent every key it finds in
/// its environment launches its sandbox all the same. The report of the
/// launch names the keys left out, never their value.
///
/// ```
/// use docker_sandbox_blueprint::KeySpec;
///
/// let key: KeySpec = serde_json::from_str(r#"{"env": "ANTHROPIC_API_KEY", "service": "anthropic"}"#)?;
/// assert_eq!(key, KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic"));
/// let key = KeySpec::for_hosts("ACME_TOKEN", ["api.acme.example"]);
/// assert_eq!(key.hosts(), ["api.acme.example"]);
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "KeyForm", into = "KeyForm")]
pub struct KeySpec {
    /// Variable of the sandbox holding the placeholder of the key.
    env: String,
    /// Service of the kits the key is written for, if it is one.
    service: Option<String>,
    /// Hosts the key is written for, when they are written out.
    hosts: Vec<String>,
    /// Whether the key is left out when it has no value or no kit declares
    /// its service.
    optional: bool,
}

/// The JSON form of a [`KeySpec`]: its variable, and its service or its
/// hosts.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyForm {
    /// Variable of the placeholder.
    env: String,
    /// Service of the kits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service: Option<String>,
    /// Hosts written out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hosts: Option<Vec<String>>,
    /// Whether the key is optional.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    optional: bool,
}

/// Where a key goes, as the report of a launch shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct KeyReport {
    /// Variable of the sandbox holding its placeholder.
    pub env: String,
    /// Hosts the proxy writes it for.
    pub hosts: Vec<String>,
}

/// A key a launch left out, as its report shows it: never its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LeftOutKey {
    /// Variable of the key.
    pub env: String,
    /// Service of the key, if it names one.
    pub service: Option<String>,
    /// Why the key was left out.
    pub reason: LeftOutReason,
}

/// Why a launch left an optional key out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LeftOutReason {
    /// The key was given no value at launch.
    NoValue,
    /// No kit of the sandbox declares the service of the key.
    UndeclaredService,
}

/// The values of the API keys of a launch, by the variable of their
/// [`KeySpec`]. Each value is a [`Secret`], redacted by [`Debug`].
///
/// ```
/// use docker_sandbox_blueprint::Keys;
///
/// let keys = Keys::new().with_key("ANTHROPIC_API_KEY", "a dummy key");
/// assert_eq!(keys.envs().collect::<Vec<_>>(), ["ANTHROPIC_API_KEY"]);
/// assert!(!format!("{keys:?}").contains("dummy"));
/// ```
#[derive(Debug, Clone, Default)]
pub struct Keys {
    /// The values, by variable.
    values: BTreeMap<String, Secret>,
}

impl KeySpec {
    /// The key of the service `service` (`anthropic` for the claude kit, …),
    /// read by the agent from the variable `env`, for the hosts the kits of
    /// the sandbox inject the key of that service into.
    #[must_use]
    pub fn for_service(env: impl Into<String>, service: impl Into<String>) -> Self {
        Self {
            env: env.into(),
            service: Some(service.into()),
            hosts: Vec::new(),
            optional: false,
        }
    }

    /// The key read by the agent from the variable `env`, for `hosts`, e.g.
    /// `api.anthropic.com`.
    #[must_use]
    pub fn for_hosts(
        env: impl Into<String>,
        hosts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            env: env.into(),
            service: None,
            hosts: hosts.into_iter().map(Into::into).collect(),
            optional: false,
        }
    }

    /// Makes the key optional: left out when it is given no value at launch,
    /// or when no kit of the sandbox declares its service, instead of
    /// failing the launch.
    ///
    /// ```
    /// use docker_sandbox_blueprint::prelude::*;
    ///
    /// // Whichever of these keys the environment holds goes to the agent.
    /// let spec = SandboxSpec::agent(Agent::OpenCode)
    ///     .with_key(KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic").optional())
    ///     .with_key(KeySpec::for_service("GROQ_API_KEY", "groq").optional());
    /// assert!(spec.check_keys(&Keys::new()).is_ok());
    /// ```
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// Whether the key is optional.
    #[must_use]
    pub fn is_optional(&self) -> bool {
        self.optional
    }

    /// Variable of the sandbox holding the placeholder of the key.
    #[must_use]
    pub fn env(&self) -> &str {
        &self.env
    }

    /// Service of the kits the key is written for, if it is one.
    #[must_use]
    pub fn service(&self) -> Option<&str> {
        self.service.as_deref()
    }

    /// Hosts the key is written for, when they are written out; those of a
    /// service are known once the kit is forged.
    #[must_use]
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }
}

impl TryFrom<KeyForm> for KeySpec {
    type Error = String;

    /// Reads a key from its JSON form, with its service or its hosts.
    fn try_from(form: KeyForm) -> Result<Self, Self::Error> {
        let key = match (form.service, form.hosts) {
            (Some(service), None) => Self::for_service(form.env, service),
            (None, Some(hosts)) => Self::for_hosts(form.env, hosts),
            _ => {
                return Err(format!(
                    "the key {} takes either a service or hosts",
                    form.env
                ));
            }
        };
        Ok(Self {
            optional: form.optional,
            ..key
        })
    }
}

impl From<KeySpec> for KeyForm {
    /// Writes the JSON form of a key.
    fn from(key: KeySpec) -> Self {
        let hosts = key.service.is_none().then_some(key.hosts);
        Self {
            env: key.env,
            service: key.service,
            hosts,
            optional: key.optional,
        }
    }
}

impl Keys {
    /// No value.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Gives `value` to the key read from the variable `env`.
    #[must_use]
    pub fn with_key(mut self, env: impl Into<String>, value: impl Into<Secret>) -> Self {
        self.values.insert(env.into(), value.into());
        self
    }

    /// The variables given a value.
    pub fn envs(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    /// Whether the variable `env` is given a value.
    pub(crate) fn contains(&self, env: &str) -> bool {
        self.values.contains_key(env)
    }

    /// The value of the variable `env`.
    pub(crate) fn get(&self, env: &str) -> Option<&Secret> {
        self.values.get(env)
    }
}

impl SandboxSpec {
    /// Gives the sandbox the API key `key`, whose value comes at launch.
    #[must_use]
    pub fn with_key(mut self, key: KeySpec) -> Self {
        self.keys.push(key);
        self
    }

    /// Checks that `keys` gives a value to every key of the spec that is not
    /// optional, and to no other variable: a key without a value cannot be
    /// pushed, and a value no key reads is most likely a misspelt variable.
    ///
    /// ```
    /// use docker_sandbox_blueprint::{KeySpec, Keys, SandboxSpec};
    /// use docker_sandbox_kit::Agent;
    ///
    /// let spec = SandboxSpec::agent(Agent::Claude)
    ///     .with_key(KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic"));
    /// assert!(spec.check_keys(&Keys::new().with_key("ANTHROPIC_API_KEY", "a dummy key")).is_ok());
    /// assert!(spec.check_keys(&Keys::new().with_key("ANTHROPIC_KEY", "a dummy key")).is_err());
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSpec`](crate::Error::InvalidSpec) for a key
    /// without a value that is not optional, or a value for a variable no
    /// key reads.
    pub fn check_keys(&self, keys: &Keys) -> Result<()> {
        if let Some(key) = self
            .keys
            .iter()
            .find(|key| !key.optional && !keys.contains(key.env()))
        {
            return Err(invalid(format!(
                "keys: the key {} is given no value at launch",
                key.env()
            )));
        }
        if let Some(env) = keys
            .envs()
            .find(|env| !self.keys.iter().any(|key| key.env() == *env))
        {
            return Err(invalid(format!(
                "keys: a value is given for {env}, which no key of the spec reads"
            )));
        }
        Ok(())
    }

    /// Checks that every key names a service, or hosts.
    pub(crate) fn check_key_targets(&self) -> Result<()> {
        for key in &self.keys {
            if key.service().is_some_and(str::is_empty) {
                return Err(invalid(format!(
                    "keys: the key {} names no service",
                    key.env()
                )));
            }
            if key.service().is_none()
                && (key.hosts().is_empty() || key.hosts().iter().any(|host| host.trim().is_empty()))
            {
                return Err(invalid(format!(
                    "keys: the key {} names no host: it would be written for none",
                    key.env()
                )));
            }
        }
        Ok(())
    }

    /// A `ProxiedKey` for each key, with the value `keys` gives it, its
    /// hosts those the kits of `package` declare for its service when it
    /// names one, and the optional keys left out. A refusal of the spec that
    /// covers one of those hosts is refused, as it would keep the key from
    /// its host.
    pub(crate) fn proxied_keys(
        &self,
        package: &KitPackage,
        keys: &Keys,
    ) -> Result<(Vec<ProxiedKey>, Vec<LeftOutKey>)> {
        // `check` read every refusal as a host pattern.
        let deny: Vec<HostPattern> = self
            .network
            .deny
            .iter()
            .filter_map(|host| HostPattern::parse(host.as_str()).ok())
            .collect();
        let mut proxied = Vec::new();
        let mut left_out = Vec::new();
        for key in &self.keys {
            let leave_out = |reason| LeftOutKey {
                env: key.env().to_owned(),
                service: key.service().map(str::to_owned),
                reason,
            };
            // `check_keys` let only an optional key go without a value.
            let Some(value) = keys.get(key.env()).cloned() else {
                left_out.push(leave_out(LeftOutReason::NoValue));
                continue;
            };
            let made = match key.service() {
                Some(service) => {
                    match ProxiedKey::for_service(package, service, key.env(), value) {
                        Ok(made) => made,
                        // The only refusal left: no kit declares the service.
                        Err(_) if key.optional => {
                            left_out.push(leave_out(LeftOutReason::UndeclaredService));
                            continue;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                None => ProxiedKey::new(key.env(), key.hosts().iter().cloned(), value)?,
            };
            if let Some((pattern, host)) = deny.iter().find_map(|pattern| {
                made.hosts()
                    .iter()
                    .find(|host| covers(pattern, host, 443))
                    .map(|host| (pattern, host))
            }) {
                return Err(invalid(format!(
                    "network.deny: {pattern} refuses {host}, where the key {} goes; a refusal \
                     wins over any allowance",
                    key.env()
                )));
            }
            proxied.push(made);
        }
        Ok((proxied, left_out))
    }
}

/// Where each of the keys `proxied` goes, as the report of a launch shows
/// it.
pub(crate) fn key_reports(proxied: &[ProxiedKey]) -> Vec<KeyReport> {
    proxied
        .iter()
        .map(|key| KeyReport {
            env: key.env().to_owned(),
            hosts: key.hosts().to_vec(),
        })
        .collect()
}

impl<K: Into<String>, V: Into<Secret>> FromIterator<(K, V)> for Keys {
    /// The values `pairs`, by variable.
    fn from_iter<I: IntoIterator<Item = (K, V)>>(pairs: I) -> Self {
        Self {
            values: pairs
                .into_iter()
                .map(|(env, value)| (env.into(), value.into()))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use docker_sandbox_kit::Agent;
    use serde_json::json;

    use super::*;
    use crate::spec::broken;

    #[test]
    fn keys_are_written_and_read_by_their_target() {
        let service = KeySpec::for_service("A", "anthropic");
        let hosts = KeySpec::for_hosts("B", ["api.example.com"]);
        assert_eq!(
            serde_json::to_value(&service).unwrap(),
            json!({ "env": "A", "service": "anthropic" })
        );
        assert_eq!(
            serde_json::to_value(&hosts).unwrap(),
            json!({ "env": "B", "hosts": ["api.example.com"] })
        );
        let optional = KeySpec::for_service("C", "groq").optional();
        assert!(optional.is_optional() && !KeySpec::for_service("C", "groq").is_optional());
        assert_eq!(
            serde_json::to_value(&optional).unwrap(),
            json!({ "env": "C", "service": "groq", "optional": true })
        );
        for key in [service, hosts, optional] {
            let json = serde_json::to_value(&key).unwrap();
            assert_eq!(serde_json::from_value::<KeySpec>(json).unwrap(), key);
        }
        for refused in [
            json!({ "env": "A" }),
            json!({ "env": "A", "service": "s", "hosts": ["h"] }),
            json!({ "env": "A", "service": "s", "value": "v" }),
        ] {
            assert!(serde_json::from_value::<KeySpec>(refused).is_err());
        }
    }

    #[test]
    fn values_are_kept_by_variable_and_never_shown() {
        let keys: Keys = [("B", "two"), ("A", "one")].into_iter().collect();
        assert_eq!(keys.envs().collect::<Vec<_>>(), ["A", "B"]);
        assert!(keys.contains("A") && !keys.contains("C"));
        assert!(!format!("{keys:?}").contains("one"));
        assert_eq!(Keys::new().with_key("A", "one").envs().count(), 1);
    }

    #[test]
    fn keys_name_a_service_or_hosts() {
        let claude = || SandboxSpec::agent(Agent::Claude);
        let service = claude().with_key(KeySpec::for_service("A", ""));
        assert!(broken(&service).contains("names no service"));
        let hosts = claude().with_key(KeySpec::for_hosts("A", Vec::<String>::new()));
        assert!(broken(&hosts).contains("names no host"));
        let blank = claude().with_key(KeySpec::for_hosts("A", [" "]));
        assert!(broken(&blank).contains("names no host"));
    }

    #[test]
    fn keys_are_given_a_value_each_and_no_other() {
        let spec = SandboxSpec::agent(Agent::Claude)
            .with_key(KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic"));
        let message = |keys: &Keys| spec.check_keys(keys).unwrap_err().to_string();
        assert!(message(&Keys::new()).contains("the key ANTHROPIC_API_KEY is given no value"));
        let extra = Keys::new()
            .with_key("ANTHROPIC_API_KEY", "a dummy key")
            .with_key("ANTHROPIC_KEY", "a dummy key");
        assert!(message(&extra).contains("a value is given for ANTHROPIC_KEY"));
        spec.check_keys(&Keys::new().with_key("ANTHROPIC_API_KEY", "a dummy key"))
            .unwrap();
        // An optional key may go without a value, and take one.
        let optional = SandboxSpec::agent(Agent::Claude)
            .with_key(KeySpec::for_service("GROQ_API_KEY", "groq").optional());
        optional.check_keys(&Keys::new()).unwrap();
        optional
            .check_keys(&Keys::new().with_key("GROQ_API_KEY", "a dummy key"))
            .unwrap();
    }
}
