//! The hosts a network rule names, checked as the `sbx` CLI checks them.
//!
//! The daemon stores any string as the resource of a rule: a host written as
//! a URL (`https://api.github.com`) or with a path (`api.github.com/v1`) is
//! accepted, listed, and never applied, so that the host it seems to name
//! stays reachable (the `network` suite of the audit). The CLI refuses those
//! before sending them; [`HostPattern`] refuses what it refuses, and
//! [`Policy::apply`](super::Policy::apply) checks every host it sends.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A host a network rule names, in one of the forms the CLI accepts.
///
/// - an exact domain, `example.com`, with globs inside its labels: `?` for one
///   character, `[12]` or `[!1]` for a class;
/// - leading wildcard labels: `*.example.com` for one label, `**.example.com`
///   for any number;
/// - an IPv4 address, or a CIDR prefix, `10.0.0.0/8` or `2001:db8::1/128`;
/// - any of those with a port, `example.com:443`; an IPv6 address takes its
///   port in brackets, `[2001:db8::1]:443`;
/// - `**`, every host.
///
/// ```
/// use docker_sandbox::policy::HostPattern;
///
/// assert!(HostPattern::parse("*.githubusercontent.com").is_ok());
/// assert!(HostPattern::parse("10.0.0.0/8").is_ok());
/// let refused = HostPattern::parse("https://api.github.com").unwrap_err();
/// assert_eq!(
///     refused.to_string(),
///     "invalid host pattern \"https://api.github.com\": a host, not a URL: leave the scheme out"
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostPattern(String);

/// A host a network rule cannot name, returned by [`HostPattern::parse`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid host pattern {pattern:?}: {reason}")]
pub struct InvalidHostPattern {
    /// The text that was given.
    pub pattern: String,
    /// Why it names no host.
    pub reason: &'static str,
}

impl HostPattern {
    /// The host pattern `pattern`.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidHostPattern`] for a text in none of the forms of
    /// [`HostPattern`]: empty, with spaces, a URL, a path, a bare `*`, an
    /// escaped glob character, a bare IPv6 address, a port that is not one.
    pub fn parse(pattern: impl Into<String>) -> Result<Self, InvalidHostPattern> {
        let pattern = pattern.into();
        match check(&pattern) {
            Ok(()) => Ok(Self(pattern)),
            Err(reason) => Err(InvalidHostPattern { pattern, reason }),
        }
    }

    /// The pattern, as the daemon receives it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for HostPattern {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for HostPattern {
    type Err = InvalidHostPattern;

    fn from_str(pattern: &str) -> Result<Self, Self::Err> {
        Self::parse(pattern)
    }
}

impl TryFrom<String> for HostPattern {
    type Error = InvalidHostPattern;

    fn try_from(pattern: String) -> Result<Self, Self::Error> {
        Self::parse(pattern)
    }
}

impl From<HostPattern> for String {
    fn from(pattern: HostPattern) -> Self {
        pattern.0
    }
}

impl From<InvalidHostPattern> for crate::Error {
    fn from(error: InvalidHostPattern) -> Self {
        Self::InvalidRequest(error.to_string())
    }
}

/// Why `pattern` names no host, if it does not.
fn check(pattern: &str) -> Result<(), &'static str> {
    if pattern.is_empty() {
        return Err("empty");
    }
    if pattern.chars().any(char::is_whitespace) {
        return Err("a host holds no space");
    }
    if pattern.contains("://") {
        return Err("a host, not a URL: leave the scheme out");
    }
    if pattern.contains('\\') {
        return Err("an escaped glob character matches nothing");
    }
    match pattern {
        "**" => return Ok(()),
        "*" => return Err("a bare * matches nothing: ** stands for every host"),
        _ => {}
    }
    if let Some(bracketed) = pattern.strip_prefix('[') {
        let (address, port) = bracketed
            .split_once("]:")
            .ok_or("an IPv6 address in brackets takes a port, as in [2001:db8::1]:443")?;
        address
            .parse::<Ipv6Addr>()
            .map_err(|_| "not an IPv6 address in brackets")?;
        return check_port(port);
    }
    if let Some((address, length)) = pattern.split_once('/') {
        return check_prefix(address, length);
    }
    if pattern.parse::<Ipv6Addr>().is_ok() {
        return Err("a bare IPv6 address: write it in brackets with a port, or as a CIDR prefix");
    }
    let host = match pattern.rsplit_once(':') {
        Some((host, port)) => {
            check_port(port)?;
            host
        }
        None => pattern,
    };
    if host.parse::<Ipv4Addr>().is_ok() {
        return Ok(());
    }
    check_domain(host)
}

/// Why `port` is not a port, if it is not.
fn check_port(port: &str) -> Result<(), &'static str> {
    match port.parse::<u16>() {
        Ok(port) if port > 0 => Ok(()),
        _ => Err("a port is a number from 1 to 65535"),
    }
}

/// Why `address/length` is not a CIDR prefix, if it is not.
fn check_prefix(address: &str, length: &str) -> Result<(), &'static str> {
    let bits = match address.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => 32,
        Ok(IpAddr::V6(_)) => 128,
        Err(_) => {
            return Err(
                "a path: a network rule names a host, and an HTTP rule takes its path apart",
            );
        }
    };
    match length.parse::<u8>() {
        Ok(length) if length <= bits => Ok(()),
        _ => Err("the length of a CIDR prefix is a number of bits of the address"),
    }
}

/// Why `host` is not a domain pattern, if it is not.
fn check_domain(host: &str) -> Result<(), &'static str> {
    let labels: Vec<&str> = host.split('.').collect();
    for (index, label) in labels.iter().enumerate() {
        match *label {
            "" => return Err("a domain has no empty label"),
            "*" | "**" if index == 0 && labels.len() > 1 => {}
            "*" | "**" => return Err("a * or ** wildcard stands for the leading labels only"),
            label => check_label(label)?,
        }
    }
    Ok(())
}

/// Why `label` is not a label of a domain pattern, if it is not.
fn check_label(label: &str) -> Result<(), &'static str> {
    let mut chars = label.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '?' => {}
            '[' => check_class(&mut chars)?,
            '*' => return Err("a * stands for whole labels, as in *.example.com"),
            _ => return Err("a character no host holds"),
        }
    }
    Ok(())
}

/// Why the character class `chars` begins, after its `[`, is not one, if it
/// is not: one or more letters, digits or hyphens, after an optional `!`.
fn check_class(chars: &mut std::str::Chars<'_>) -> Result<(), &'static str> {
    let mut members = 0;
    let mut first = true;
    for c in chars.by_ref() {
        match c {
            ']' if members > 0 => return Ok(()),
            '!' if first => {}
            c if c.is_ascii_alphanumeric() || c == '-' => members += 1,
            _ => break,
        }
        first = false;
    }
    Err("a character class is [ then letters, digits or hyphens, after an optional !, then ]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_forms_the_cli_accepts_are_accepted() {
        for pattern in [
            "example.com",
            "*.example.com",
            "**.example.com",
            "api?.example.com",
            "api[12].example.com",
            "api[!1].example.com",
            "api-v2_x.example.com",
            "example.com:443",
            "*.example.com:8443",
            "10.0.0.1",
            "10.0.0.1:80",
            "10.0.0.0/8",
            "2001:db8::1/128",
            "[2001:db8::1]:443",
            "localhost",
            "**",
        ] {
            let parsed = HostPattern::parse(pattern).unwrap();
            assert_eq!(parsed.as_str(), pattern);
            assert_eq!(parsed.to_string(), pattern);
            assert_eq!(parsed.as_ref(), pattern);
            assert_eq!(pattern.parse::<HostPattern>().unwrap(), parsed);
            assert_eq!(String::from(parsed), pattern);
        }
    }

    #[test]
    fn the_forms_the_cli_refuses_are_refused_with_their_reason() {
        for (pattern, reason) in [
            ("", "empty"),
            ("api .example.com", "a host holds no space"),
            ("https://api.github.com", "a host, not a URL"),
            (r"\*.example.com", "an escaped glob character"),
            ("*", "a bare * matches nothing"),
            ("[2001:db8::1]", "takes a port"),
            ("[example.com]:443", "not an IPv6 address"),
            ("[2001:db8::1]:0", "a port is a number"),
            ("api.github.com/v1", "a path"),
            ("10.0.0.0/33", "the length of a CIDR prefix"),
            ("2001:db8::1/x", "the length of a CIDR prefix"),
            ("2001:db8::1", "a bare IPv6 address"),
            ("example.com:99999", "a port is a number"),
            ("example.com:", "a port is a number"),
            ("example..com", "no empty label"),
            ("example.com.", "no empty label"),
            ("api.*.example.com", "the leading labels only"),
            ("**.", "no empty label"),
            ("api*.example.com", "whole labels"),
            ("api!.example.com", "a character no host holds"),
            ("api[].example.com", "a character class"),
            ("api[12.example.com", "a character class"),
            ("api[1.2].example.com", "a character class"),
            ("api[1!].example.com", "a character class"),
        ] {
            let error = HostPattern::parse(pattern).unwrap_err();
            assert_eq!(error.pattern, pattern);
            assert!(error.reason.contains(reason), "{pattern}: {error}");
            assert!(
                error.to_string().starts_with("invalid host pattern"),
                "{error}"
            );
        }
    }

    #[test]
    fn patterns_travel_as_strings_and_are_checked_on_the_way_in() {
        let pattern = HostPattern::parse("*.example.com").unwrap();
        let json = serde_json::to_value(&pattern).unwrap();
        assert_eq!(json, "*.example.com");
        assert_eq!(
            serde_json::from_value::<HostPattern>(json).unwrap(),
            pattern
        );
        let error =
            serde_json::from_value::<HostPattern>(serde_json::json!("https://x")).unwrap_err();
        assert!(error.to_string().contains("not a URL"), "{error}");
        assert_eq!(
            HostPattern::try_from("**".to_owned()).unwrap().as_str(),
            "**"
        );
    }
}
