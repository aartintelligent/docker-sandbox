//! Network rules: the rules in force and the changes that add or remove them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Extra;
use crate::error::{Error, Result};

/// Rules in force, from `GET /policy/network/rules?type=all`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkRules {
    /// Whether an organization policy governs the rules.
    #[serde(default)]
    pub governance_active: bool,
    /// Every rule, global and sandbox-scoped.
    #[serde(default)]
    pub rules: Vec<NetworkRule>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A rule of the policy: network, HTTP or file system access.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkRule {
    /// Rule identifier; generated for added rules.
    pub id: String,
    /// Rule name, the identifier for added rules.
    #[serde(default)]
    pub name: String,
    /// Whether matching traffic is allowed or denied.
    pub decision: Decision,
    /// Actions the rule governs, e.g. `net:connect:tcp` or `http:request:get`.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Hosts or patterns the rule applies to; `**` matches everything.
    #[serde(default)]
    pub resources: Vec<String>,
    /// Kind of resource: `network`, `http`, `filesystem:read`, …
    #[serde(default)]
    pub resource_type: Option<String>,
    /// `global`, or `sandbox:<name>` for a sandbox-scoped rule.
    #[serde(default)]
    pub scope: Option<String>,
    /// What the rule applies to: `all`, or `sandbox:<name>`.
    #[serde(default)]
    pub applies_to: Option<String>,
    /// Sandbox of a sandbox-scoped rule.
    #[serde(default)]
    pub sandbox_id: Option<String>,
    /// HTTP methods of an HTTP rule.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Hosts and paths of an HTTP rule.
    #[serde(default)]
    pub http_targets: Vec<HttpTarget>,
    /// Whether the rule can be changed or removed.
    #[serde(default)]
    pub editable: bool,
    /// Where the rule comes from: `local`, `scoped`, …
    #[serde(default)]
    pub origin: Option<String>,
    /// Policy layer of the rule.
    #[serde(default)]
    pub layer: Option<String>,
    /// Policy the rule belongs to.
    #[serde(default)]
    pub policy_id: Option<String>,
    /// Rule state, e.g. `active`.
    #[serde(default)]
    pub status: Option<String>,
    /// How the rule was created.
    #[serde(default)]
    pub provenance: Option<Value>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl NetworkRule {
    /// Whether the rule is scoped to the sandbox named `name`.
    #[must_use]
    pub fn is_for_sandbox(&self, name: &str) -> bool {
        self.sandbox_id.as_deref() == Some(name)
    }
}

/// Outcome of a rule.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
#[non_exhaustive]
pub enum Decision {
    /// Matching traffic is allowed.
    Allow,
    /// Matching traffic is denied.
    Deny,
    /// Any other decision reported by the daemon.
    Other(String),
}

impl From<String> for Decision {
    fn from(s: String) -> Self {
        match s.as_str() {
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            _ => Self::Other(s),
        }
    }
}

impl From<Decision> for String {
    fn from(decision: Decision) -> Self {
        match decision {
            Decision::Allow => "allow".to_owned(),
            Decision::Deny => "deny".to_owned(),
            Decision::Other(s) => s,
        }
    }
}

/// Host and path of an HTTP rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpTarget {
    /// Host.
    pub host: String,
    /// Path pattern, e.g. `/v1/**`.
    #[serde(default)]
    pub path: Option<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A change to the rules, sent to `POST /policy/network/rules`.
///
/// The constructors mirror the defaults of the `sbx` CLI: an allow rule covers
/// TCP, a deny rule TCP and UDP. A change is global unless
/// [`RuleChange::for_sandbox`] scopes it to one sandbox.
///
/// ```
/// use docker_sandbox::policy::RuleChange;
///
/// let changes = [
///     RuleChange::allow(["api.github.com", "*.githubusercontent.com"]).for_sandbox("job-42"),
///     RuleChange::allow_http(["api.example.com"])
///         .with_methods(["GET"])
///         .with_path("/v1/**")
///         .for_sandbox("job-42"),
///     RuleChange::deny(["tracker.example.com"]).for_sandbox("job-42"),
/// ];
/// # let _ = changes;
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleChange {
    /// What the change does.
    pub action: RuleAction,
    /// Hosts or patterns it applies to; empty for a removal by identifier.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<String>,
    /// Rule removed by [`RuleChange::remove_rule`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Protocols of an allow or deny rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protocols: Vec<Protocol>,
    /// HTTP methods of an HTTP rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// Path pattern of an HTTP rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Sandbox the change is scoped to; global when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_id: Option<String>,
}

impl RuleChange {
    /// A change of `action` on `resources`.
    fn new(action: RuleAction, resources: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            action,
            resources: resources.into_iter().map(Into::into).collect(),
            id: None,
            protocols: Vec::new(),
            methods: Vec::new(),
            path: None,
            sandbox_id: None,
        }
    }

    /// Allows TCP connections to `resources`.
    pub fn allow(resources: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::new(RuleAction::Allow, resources).with_protocols([Protocol::Tcp])
    }

    /// Denies TCP and UDP connections to `resources`.
    pub fn deny(resources: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::new(RuleAction::Deny, resources).with_protocols([Protocol::Tcp, Protocol::Udp])
    }

    /// Allows HTTP requests to `resources`, narrowed with
    /// [`RuleChange::with_methods`] and [`RuleChange::with_path`].
    pub fn allow_http(resources: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::new(RuleAction::AllowHttp, resources)
    }

    /// Denies HTTP requests to `resources`, narrowed with
    /// [`RuleChange::with_methods`] and [`RuleChange::with_path`].
    pub fn deny_http(resources: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::new(RuleAction::DenyHttp, resources)
    }

    /// Removes the rules added for `resources`. Removing an HTTP rule also
    /// takes its [`RuleChange::with_path`].
    pub fn remove(resources: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::new(RuleAction::Remove, resources)
    }

    /// Removes the rule whose identifier is `id`, as listed by
    /// [`Policy::rules`](super::Policy::rules) or returned in
    /// [`CreatedRule::rule_id`]. A rule scoped to a sandbox is only found with
    /// [`RuleChange::for_sandbox`].
    pub fn remove_rule(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            ..Self::new(RuleAction::RemoveRule, Vec::<String>::new())
        }
    }

    /// Scopes the change to the sandbox named `name`.
    #[must_use]
    pub fn for_sandbox(mut self, name: impl Into<String>) -> Self {
        self.sandbox_id = Some(name.into());
        self
    }

    /// Replaces the protocols of an allow or deny rule.
    #[must_use]
    pub fn with_protocols(mut self, protocols: impl IntoIterator<Item = Protocol>) -> Self {
        self.protocols = protocols.into_iter().collect();
        self
    }

    /// Restricts an HTTP rule to `methods`, e.g. `["GET", "POST"]`.
    #[must_use]
    pub fn with_methods(mut self, methods: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.methods = methods.into_iter().map(Into::into).collect();
        self
    }

    /// Restricts an HTTP rule to the path pattern `path`, e.g. `/v1/**`.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// What a [`RuleChange`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RuleAction {
    /// Adds an allow rule (`allow`).
    #[serde(rename = "allow")]
    Allow,
    /// Adds a deny rule (`deny`).
    #[serde(rename = "deny")]
    Deny,
    /// Adds an HTTP allow rule (`http-allow`).
    #[serde(rename = "http-allow")]
    AllowHttp,
    /// Adds an HTTP deny rule (`http-deny`).
    #[serde(rename = "http-deny")]
    DenyHttp,
    /// Removes the rules added for the resources (`remove-resource`).
    #[serde(rename = "remove-resource")]
    Remove,
    /// Removes one rule by identifier (`remove-id`).
    #[serde(rename = "remove-id")]
    RemoveRule,
}

/// Transport protocol of a network rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Protocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
}

/// Outcome of one [`RuleChange`].
///
/// The daemon answers a batch of changes with HTTP 200 and reports the failure
/// of one change in its [`RuleChangeResult::error`]: check it, or call
/// [`RuleChangeResult::into_result`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleChangeResult {
    /// Action that was applied, as the daemon names it; empty for an unknown
    /// action.
    #[serde(default)]
    pub action: String,
    /// Why the change failed, e.g. an unknown rule identifier.
    #[serde(default)]
    pub error: Option<String>,
    /// Rule identifier of a removal by identifier.
    #[serde(default)]
    pub id: Option<String>,
    /// Resources it was applied to.
    #[serde(default)]
    pub resources: Vec<String>,
    /// Sandbox it was scoped to.
    #[serde(default)]
    pub sandbox_id: Option<String>,
    /// Rules created by the change.
    #[serde(default)]
    pub created: Vec<CreatedRule>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl RuleChangeResult {
    /// Whether the change was applied.
    #[must_use]
    pub fn is_applied(&self) -> bool {
        self.error.is_none()
    }

    /// Returns the result when the change was applied.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Failed`] with the reason given by the daemon when the
    /// change failed.
    pub fn into_result(self) -> Result<Self> {
        match self.error {
            Some(message) => Err(Error::Failed {
                operation: "network rule change",
                message,
            }),
            None => Ok(self),
        }
    }
}

/// A rule created by a [`RuleChange`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatedRule {
    /// Identifier of the new rule.
    pub rule_id: String,
    /// Resource the rule covers.
    pub resource: String,
    /// Protocols of the rule.
    #[serde(default)]
    pub protocols: Vec<String>,
    /// Unknown fields.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Response of `POST /policy/network/rules`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RuleChangeResponse {
    /// One outcome per change, in order.
    pub(crate) results: Vec<RuleChangeResult>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn changes_serialize_as_the_cli_sends_them() {
        let allow = RuleChange::allow(["a.example.com"]).for_sandbox("job");
        assert_eq!(
            serde_json::to_value(&allow).unwrap(),
            json!({"action": "allow", "protocols": ["tcp"], "resources": ["a.example.com"], "sandbox_id": "job"})
        );
        let http = RuleChange::allow_http(["h.example.com"])
            .with_methods(["GET"])
            .with_path("/v1/**");
        assert_eq!(
            serde_json::to_value(&http).unwrap(),
            json!({"action": "http-allow", "methods": ["GET"], "path": "/v1/**", "resources": ["h.example.com"]})
        );
        assert_eq!(
            serde_json::to_value(RuleChange::deny(["d"])).unwrap()["protocols"],
            json!(["tcp", "udp"])
        );
        assert_eq!(
            serde_json::to_value(RuleChange::remove(["r"])).unwrap(),
            json!({"action": "remove-resource", "resources": ["r"]})
        );
        assert_eq!(
            serde_json::to_value(RuleChange::remove_rule("r1")).unwrap(),
            json!({"action": "remove-id", "id": "r1"})
        );
        assert_eq!(
            serde_json::to_value(
                RuleChange::deny_http(["h"])
                    .with_methods(["POST"])
                    .with_path("/admin/**")
            )
            .unwrap(),
            json!({"action": "http-deny", "methods": ["POST"], "path": "/admin/**", "resources": ["h"]})
        );
    }

    #[test]
    fn failed_changes_are_reported_in_their_result() {
        let failed: RuleChangeResult = serde_json::from_value(json!({
            "action": "remove-id",
            "error": "remove-id: not found: rule \"zz\"",
            "id": "zz"
        }))
        .unwrap();
        assert!(!failed.is_applied());
        assert!(
            matches!(failed.into_result(), Err(Error::Failed { ref message, .. }) if message.contains("not found"))
        );
    }
}
