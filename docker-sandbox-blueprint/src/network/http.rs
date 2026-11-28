//! The HTTP rules of a spec: methods and paths allowed or refused on hosts.
//!
//! A `network-policy@1` mixin holds hosts only, so the HTTP rules of a spec
//! are not part of its kit: the launch applies them through the policy API
//! of the daemon once the sandbox runs, scoped to the sandbox, as
//! `sbx policy allow network --sandbox <name> <host> --method … --path …`
//! does. They go with the sandbox (the `network` suite of the audit). As
//! they come after the start, they do not cover the install commands. Each
//! rule names its methods: a rule without one was not measured.

use docker_sandbox::policy::{HostPattern, RuleChange};
use serde::{Deserialize, Serialize};

use crate::error::{Result, invalid};
use crate::spec::SandboxSpec;

/// The path an HTTP rule covers when it names none: every path.
const EVERY_PATH: &str = "/**";

/// An HTTP rule of a spec: the hosts, methods and path it allows or
/// refuses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HttpRule {
    /// Hosts, as host patterns.
    pub(crate) hosts: Vec<String>,
    /// Methods, e.g. `GET`.
    pub(crate) methods: Vec<String>,
    /// Path pattern, e.g. `/v1/**`.
    #[serde(default = "every_path")]
    pub(crate) path: String,
}

/// An HTTP rule of a launch, as its report shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct HttpRuleReport {
    /// Hosts of the rule.
    pub hosts: Vec<String>,
    /// Its methods.
    pub methods: Vec<String>,
    /// Its path pattern.
    pub path: String,
}

impl SandboxSpec {
    /// Allows the HTTP requests of `methods` (`GET`, `POST`, …) to `path`
    /// (`/v1/**`, `/**` for every path) on `hosts`. Applied once the sandbox
    /// runs, the rule does not cover the install commands; as an allowance,
    /// it restricts nothing while the global policy of the daemon allows
    /// every host.
    #[must_use]
    pub fn with_http_allow(
        mut self,
        hosts: impl IntoIterator<Item = impl Into<String>>,
        methods: impl IntoIterator<Item = impl Into<String>>,
        path: impl Into<String>,
    ) -> Self {
        let rule = HttpRule::new(hosts, methods, path);
        self.network.http_allow.push(rule);
        self
    }

    /// Refuses the HTTP requests of `methods` to `path` on `hosts`, whatever
    /// else allows them. Applied once the sandbox runs, the rule does not
    /// cover the install commands.
    #[must_use]
    pub fn with_http_deny(
        mut self,
        hosts: impl IntoIterator<Item = impl Into<String>>,
        methods: impl IntoIterator<Item = impl Into<String>>,
        path: impl Into<String>,
    ) -> Self {
        let rule = HttpRule::new(hosts, methods, path);
        self.network.http_deny.push(rule);
        self
    }

    /// Checks that every HTTP rule names host patterns, methods and a path.
    pub(crate) fn check_http_rules(&self) -> Result<()> {
        let rules = [
            ("network.http_allow", &self.network.http_allow),
            ("network.http_deny", &self.network.http_deny),
        ];
        for (field, rules) in rules {
            for rule in rules {
                rule.check(field)?;
            }
        }
        Ok(())
    }

    /// The changes that apply the HTTP rules of the spec to the sandbox
    /// `sandbox`.
    pub(crate) fn http_changes(&self, sandbox: &str) -> Vec<RuleChange> {
        let allow = self
            .network
            .http_allow
            .iter()
            .map(|rule| (RuleChange::allow_http(rule.hosts.clone()), rule));
        let deny = self
            .network
            .http_deny
            .iter()
            .map(|rule| (RuleChange::deny_http(rule.hosts.clone()), rule));
        allow
            .chain(deny)
            .map(|(change, rule)| {
                change
                    .with_methods(rule.methods.clone())
                    .with_path(rule.path.clone())
                    .for_sandbox(sandbox)
            })
            .collect()
    }
}

impl HttpRule {
    /// The rule of `methods` to `path` on `hosts`.
    fn new(
        hosts: impl IntoIterator<Item = impl Into<String>>,
        methods: impl IntoIterator<Item = impl Into<String>>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            hosts: hosts.into_iter().map(Into::into).collect(),
            methods: methods.into_iter().map(Into::into).collect(),
            path: path.into(),
        }
    }

    /// Checks the rule, written under `field`.
    fn check(&self, field: &str) -> Result<()> {
        if self.hosts.is_empty() {
            return Err(invalid(format!("{field}: a rule names no host")));
        }
        for host in &self.hosts {
            HostPattern::parse(host.as_str())
                .map_err(|error| invalid(format!("{field}: {error}")))?;
        }
        if self.methods.is_empty() || self.methods.iter().any(|method| method.trim().is_empty()) {
            return Err(invalid(format!(
                "{field}: a rule on {} names no method",
                self.hosts.join(", ")
            )));
        }
        if !self.path.starts_with('/') {
            return Err(invalid(format!(
                "{field}: {:?} is not a path pattern, which starts with /",
                self.path
            )));
        }
        Ok(())
    }

    /// The rule, as the report of a launch shows it.
    pub(crate) fn report(&self) -> HttpRuleReport {
        HttpRuleReport {
            hosts: self.hosts.clone(),
            methods: self.methods.clone(),
            path: self.path.clone(),
        }
    }
}

/// The path of an HTTP rule that names none.
fn every_path() -> String {
    EVERY_PATH.to_owned()
}

#[cfg(test)]
mod tests {
    use docker_sandbox_kit::Agent;
    use serde_json::json;

    use super::*;
    use crate::spec::broken;

    /// A shell spec.
    fn shell() -> SandboxSpec {
        SandboxSpec::agent(Agent::Shell)
    }

    #[test]
    fn http_rules_are_written_and_read_as_json() {
        let spec = shell()
            .with_http_allow(["api.example.com"], ["GET"], "/v1/**")
            .with_http_deny(["api.example.com"], ["POST", "DELETE"], "/admin/**");
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(
            json["network"],
            json!({
                "http_allow": [{ "hosts": ["api.example.com"], "methods": ["GET"], "path": "/v1/**" }],
                "http_deny": [{ "hosts": ["api.example.com"], "methods": ["POST", "DELETE"], "path": "/admin/**" }]
            })
        );
        assert_eq!(serde_json::from_value::<SandboxSpec>(json).unwrap(), spec);
        // A rule that names no path covers every path.
        let read: SandboxSpec = serde_json::from_value(json!({
            "agent": "shell",
            "network": { "http_allow": [{ "hosts": ["a.example.com"], "methods": ["GET"] }] }
        }))
        .unwrap();
        assert_eq!(read.network.http_allow[0].path, "/**");
        // HTTP rules alone make no mixin: the launch applies them.
        assert!(read.network_mixin().unwrap().is_none());
    }

    #[test]
    fn http_rules_name_hosts_methods_and_a_path() {
        let no_host = shell().with_http_allow(Vec::<String>::new(), ["GET"], "/**");
        assert!(broken(&no_host).starts_with("network.http_allow: a rule names no host"));
        let url = shell().with_http_deny(["https://api.example.com"], ["GET"], "/**");
        assert!(broken(&url).starts_with("network.http_deny: invalid host pattern"));
        let no_method = shell().with_http_allow(["api.example.com"], Vec::<String>::new(), "/**");
        assert!(broken(&no_method).contains("names no method"));
        let blank = shell().with_http_allow(["api.example.com"], [" "], "/**");
        assert!(broken(&blank).contains("names no method"));
        let path = shell().with_http_allow(["api.example.com"], ["GET"], "v1/**");
        assert!(broken(&path).contains("is not a path pattern"));
    }

    #[test]
    fn http_rules_become_changes_scoped_to_the_sandbox() {
        let spec = shell()
            .with_http_allow(["api.example.com"], ["GET"], "/v1/**")
            .with_http_deny(["api.example.com"], ["POST"], "/**");
        let changes = serde_json::to_value(spec.http_changes("job-42")).unwrap();
        assert_eq!(
            changes,
            json!([
                { "action": "http-allow", "resources": ["api.example.com"], "methods": ["GET"], "path": "/v1/**", "sandbox_id": "job-42" },
                { "action": "http-deny", "resources": ["api.example.com"], "methods": ["POST"], "path": "/**", "sandbox_id": "job-42" }
            ])
        );
    }
}
