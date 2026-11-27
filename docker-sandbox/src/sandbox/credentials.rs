//! Credentials pushed to a sandbox (`POST /sandbox/{name}/credentials`).
//!
//! Values travel in clear text over the socket and are never readable back from
//! the daemon. On this side, a [`Secret`] wipes its value from memory when it
//! is dropped, and so does the body of a credentials request once sent: the
//! copies this crate makes do not outlive their use (those of the caller, and
//! the buffers of the operating system, are out of its reach). Inside the sandbox, the agent only sees placeholders that the
//! sandbox proxy replaces with the real value in the headers of its outgoing
//! requests. What a push carries stays with the sandbox, through restarts,
//! until another push changes it or the sandbox goes: a push of custom secrets
//! replaces the whole list of the sandbox, a push without them leaves it.
//!
//! A credential that a kit of the sandbox declares is injected only when the
//! bindings file the sandbox was created with authorizes it
//! ([`CreateRequest::with_bindings_path`](super::CreateRequest::with_bindings_path)).
//! Otherwise the daemon withholds it, accepts a value pushed for its service
//! all the same, and names the service in [`CredentialsOutcome::withheld`]:
//! the value is not injected. Custom secrets, which target hosts rather than
//! declared services, do not depend on bindings.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

use super::Sandbox;
use crate::Extra;
use crate::error::Result;
use crate::transport::Request;

/// A secret value: redacted by [`Debug`], wiped from memory when dropped, and
/// serialized as it is, since the daemon takes it in clear text.
///
/// It has no [`Display`](fmt::Display) and no equality: reading it is
/// explicit, with [`Secret::expose_secret`].
///
/// ```
/// use docker_sandbox::sandbox::Secret;
///
/// let secret = Secret::new("s3cr3t");
/// assert_eq!(format!("{secret:?}"), "Secret(***)");
/// assert_eq!(secret.expose_secret(), "s3cr3t");
/// ```
#[derive(Clone)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Returns the underlying value.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[cfg(feature = "secrecy")]
#[cfg_attr(docsrs, doc(cfg(feature = "secrecy")))]
impl From<secrecy::SecretString> for Secret {
    /// Takes the value of a [`secrecy::SecretString`], which wipes its own
    /// copy when dropped.
    fn from(value: secrecy::SecretString) -> Self {
        use secrecy::ExposeSecret;
        Self::new(value.expose_secret())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Body of `POST /sandbox/{name}/credentials`.
///
/// An empty update (`{}`) is valid: the `sbx` CLI sends one before every
/// operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialsUpdate {
    /// Proxy-injected secrets: `None` leaves those of the sandbox as they are,
    /// a list replaces them all, and an empty list removes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_secrets: Option<Vec<CustomSecret>>,
    /// Credentials for services known to `sbx`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_values: Vec<ServiceCredential>,
    /// How each service credential is resolved, keyed by service.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub credential_sources: BTreeMap<String, CredentialSource>,
    /// Services whose credential source is removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_credential_sources: Vec<String>,
}

impl CredentialsUpdate {
    /// Whether the update carries no change.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.custom_secrets.is_none()
            && self.credential_values.is_empty()
            && self.credential_sources.is_empty()
            && self.removed_credential_sources.is_empty()
    }

    /// Adds a proxy-injected secret.
    ///
    /// ```
    /// use docker_sandbox::sandbox::{CredentialSource, CredentialsUpdate, CustomSecret};
    ///
    /// let update = CredentialsUpdate::default()
    ///     .with_custom_secret(
    ///         CustomSecret::new(["api.example.com"], "sbx-cs-1234", "s3cr3t").with_env_name("API_KEY"),
    ///     )
    ///     .with_service_credential("github", "ghp_example")
    ///     .with_credential_source("github", CredentialSource::env("SANDBOX_GITHUB_TOKEN"));
    /// assert!(!update.is_empty());
    /// ```
    ///
    /// The custom secrets of an update replace those of the sandbox: give them
    /// all in one update.
    #[must_use]
    pub fn with_custom_secret(mut self, secret: CustomSecret) -> Self {
        self.custom_secrets
            .get_or_insert_with(Vec::new)
            .push(secret);
        self
    }

    /// Removes every custom secret of the sandbox, as an empty list.
    ///
    /// ```
    /// use docker_sandbox::sandbox::CredentialsUpdate;
    ///
    /// let update = CredentialsUpdate::default().without_custom_secrets();
    /// assert_eq!(
    ///     serde_json::to_value(&update)?,
    ///     serde_json::json!({ "custom_secrets": [] })
    /// );
    /// # Ok::<(), serde_json::Error>(())
    /// ```
    #[must_use]
    pub fn without_custom_secrets(mut self) -> Self {
        self.custom_secrets = Some(Vec::new());
        self
    }

    /// Sets the credential of `service`. An empty value clears it.
    #[must_use]
    pub fn with_service_credential(
        mut self,
        service: impl Into<String>,
        value: impl Into<Secret>,
    ) -> Self {
        self.credential_values
            .push(ServiceCredential::new(service, value));
        self
    }

    /// Sets how the credential of `service` is resolved.
    #[must_use]
    pub fn with_credential_source(
        mut self,
        service: impl Into<String>,
        source: CredentialSource,
    ) -> Self {
        self.credential_sources.insert(service.into(), source);
        self
    }

    /// Removes the credential source of `service`.
    #[must_use]
    pub fn without_credential_source(mut self, service: impl Into<String>) -> Self {
        self.removed_credential_sources.push(service.into());
        self
    }

    /// Sets the credential of `service` for the sandbox `sandbox`, the way
    /// `sbx secret set SERVICE --sandbox SANDBOX` does: the value, and an
    /// environment source named `SANDBOX_STORED_CREDENTIAL_<SANDBOX>_<SERVICE>`.
    ///
    /// ```
    /// use docker_sandbox::sandbox::CredentialsUpdate;
    ///
    /// let update = CredentialsUpdate::default().with_sandbox_secret("job-42", "anthropic", "sk-…");
    /// assert_eq!(
    ///     update.credential_sources["anthropic"].source,
    ///     "SANDBOX_STORED_CREDENTIAL_JOB_42_ANTHROPIC"
    /// );
    /// ```
    #[must_use]
    pub fn with_sandbox_secret(
        self,
        sandbox: &str,
        service: impl Into<String>,
        value: impl Into<Secret>,
    ) -> Self {
        let service = service.into();
        let source = format!(
            "SANDBOX_STORED_CREDENTIAL_{}_{}",
            env_name_part(sandbox),
            env_name_part(&service)
        );
        self.with_service_credential(service.clone(), value)
            .with_credential_source(service, CredentialSource::env(source))
    }

    /// Removes the credential of `service` from a sandbox, the way
    /// `sbx secret rm SERVICE --sandbox SANDBOX` does.
    #[must_use]
    pub fn without_sandbox_secret(self, service: impl Into<String>) -> Self {
        let service = service.into();
        self.with_service_credential(service.clone(), "")
            .without_credential_source(service)
    }
}

/// `name` as a part of an environment variable name: upper case, every
/// character other than a letter or a digit replaced with `_`.
fn env_name_part(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// A secret injected by the sandbox proxy into requests to `targets`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomSecret {
    /// Environment variable that holds the placeholder inside the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_name: Option<String>,
    /// Placeholder visible inside the sandbox.
    pub sentinel: String,
    /// Primary target host.
    pub target: String,
    /// Every host or pattern the real value is sent to.
    pub targets: Vec<String>,
    /// The real value.
    pub value: Secret,
}

impl CustomSecret {
    /// A secret sent to `targets` in place of the placeholder `sentinel`. The
    /// first target is the primary one.
    pub fn new<I, S>(targets: I, sentinel: impl Into<String>, value: impl Into<Secret>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let targets: Vec<String> = targets.into_iter().map(Into::into).collect();
        Self {
            env_name: None,
            sentinel: sentinel.into(),
            target: targets.first().cloned().unwrap_or_default(),
            targets,
            value: value.into(),
        }
    }

    /// Exposes the placeholder as the environment variable `name`.
    #[must_use]
    pub fn with_env_name(mut self, name: impl Into<String>) -> Self {
        self.env_name = Some(name.into());
        self
    }
}

/// A credential for a service known to `sbx`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceCredential {
    /// Service identifier, e.g. `github`.
    pub service: String,
    /// The credential. Empty to clear it.
    pub value: Secret,
}

impl ServiceCredential {
    /// The credential `value` for `service`.
    pub fn new(service: impl Into<String>, value: impl Into<Secret>) -> Self {
        Self {
            service: service.into(),
            value: value.into(),
        }
    }

    /// Clears the credential of `service`.
    pub fn cleared(service: impl Into<String>) -> Self {
        Self::new(service, "")
    }
}

/// How a service credential is resolved inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialSource {
    /// Source identifier, e.g. an environment variable name.
    pub source: String,
    /// Source kind, e.g. `env`.
    #[serde(rename = "type")]
    pub kind: String,
}

impl CredentialSource {
    /// A credential read from the environment variable `name`, as the `sbx`
    /// CLI declares stored credentials.
    pub fn env(name: impl Into<String>) -> Self {
        Self {
            source: name.into(),
            kind: "env".to_owned(),
        }
    }
}

/// Response of `POST /sandbox/{name}/credentials`.
///
/// The daemon accepts an update even when it withholds what it carries: read
/// [`CredentialsOutcome::withheld`] to know which services get nothing.
///
/// ```
/// use docker_sandbox::sandbox::CredentialsOutcome;
///
/// let outcome: CredentialsOutcome = serde_json::from_value(serde_json::json!({
///     "message": "Credentials updated",
///     "credential_source_updates_applied": true,
///     "withheld": ["example"]
/// }))?;
/// assert_eq!(outcome.withheld, ["example"]);
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CredentialsOutcome {
    /// Human-readable message, e.g. `Credentials updated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Services whose credential, declared by a kit of the sandbox, the
    /// daemon withholds because no bindings file authorizes it: the proxy
    /// injects nothing for them, a value just pushed included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<String>,
    /// Whether the credential sources of the update were applied; absent
    /// when the update carries none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_source_updates_applied: Option<bool>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Sandbox {
    /// Pushes credentials to the sandbox proxy
    /// (`POST /sandbox/{name}/credentials`), and returns what the daemon
    /// made of them.
    ///
    /// Custom secrets replace those pushed before; service credentials and
    /// sources are updated one by one. A service the outcome lists as
    /// withheld gets no credential whatever this update carries.
    ///
    /// # Errors
    ///
    /// Fails if the sandbox does not exist or the update is rejected.
    pub async fn push_credentials(&self, update: &CredentialsUpdate) -> Result<CredentialsOutcome> {
        let request =
            Request::post("credentials update", self.path("credentials")).secret_json(update)?;
        self.transport().json(request).await
    }

    /// Gives this sandbox the credential of `service` (`anthropic`,
    /// `openai`, `github`, …), as `sbx secret set SERVICE --sandbox` does.
    /// The proxy of the sandbox injects it on outgoing requests; the value is
    /// never stored by the daemon nor readable back.
    ///
    /// When a kit of the sandbox declares a credential for `service` and no
    /// bindings file authorizes it, the daemon keeps it withheld and the
    /// outcome names the service: the value is not injected.
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) if the sandbox
    /// does not exist.
    pub async fn push_service_secret(
        &self,
        service: &str,
        value: impl Into<Secret>,
    ) -> Result<CredentialsOutcome> {
        let update = CredentialsUpdate::default().with_sandbox_secret(self.name(), service, value);
        self.push_credentials(&update).await
    }

    /// Takes the credential of `service` back from this sandbox, as
    /// `sbx secret rm SERVICE --sandbox` does.
    ///
    /// # Errors
    ///
    /// Fails with a not-found [`Error::Api`](crate::Error::Api) if the sandbox
    /// does not exist.
    pub async fn remove_service_secret(&self, service: &str) -> Result<CredentialsOutcome> {
        let update = CredentialsUpdate::default().without_sandbox_secret(service);
        self.push_credentials(&update).await
    }
}
