//! Whether the host pattern of a network rule covers a host.
//!
//! A refusal always wins over an allowance, the rules of the sandbox and of
//! its kits alike (the `network` suite of the audit): a host a refusal covers
//! is out of reach whatever else allows it. A spec whose refusals cover the
//! host of one of its keys or MCP servers launches a sandbox that cannot use
//! them, so the check refuses it first. The patterns are read as
//! [`HostPattern`] describes them, and only what a pattern certainly covers
//! counts: a doubt never refuses a spec.

use std::net::IpAddr;

use docker_sandbox::policy::HostPattern;

/// Whether `pattern` covers `host` on `port`.
pub(crate) fn covers(pattern: &HostPattern, host: &str, port: u16) -> bool {
    let text = pattern.as_str();
    if text == "**" {
        return true;
    }
    let (name, rule_port) = split_port(text);
    if rule_port.is_some_and(|rule_port| rule_port != port) {
        return false;
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let name = name.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<IpAddr>() {
        Ok(address) => covers_address(name, address),
        Err(_) => covers_domain(name, host),
    }
}

/// The host of `authority` (`host`, `host:port`, `[v6]:port`) and its port,
/// `default` when it names none.
pub(crate) fn authority(authority: &str, default: u16) -> (&str, u16) {
    match split_port(authority) {
        (host, Some(port)) => (host, port),
        (host, None) => (host, default),
    }
}

/// The pattern or authority `text` without its port, and the port.
fn split_port(text: &str) -> (&str, Option<u16>) {
    if text.starts_with('[') {
        return match text.split_once("]:") {
            Some((address, port)) => (&text[..=address.len()], port.parse().ok()),
            None => (text, None),
        };
    }
    match text.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') => match port.parse() {
            Ok(port) => (name, Some(port)),
            Err(_) => (text, None),
        },
        _ => (text, None),
    }
}

/// Whether the address or prefix `name` covers `address`.
fn covers_address(name: &str, address: IpAddr) -> bool {
    let (network, bits) = match name.split_once('/') {
        Some((network, bits)) => match bits.parse::<u32>() {
            Ok(bits) => (network, bits),
            Err(_) => return false,
        },
        None => (name, 128),
    };
    match (network.parse::<IpAddr>(), address) {
        (Ok(IpAddr::V4(network)), IpAddr::V4(address)) => {
            let bits = bits.min(32);
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            u32::from(network) & mask == u32::from(address) & mask
        }
        (Ok(IpAddr::V6(network)), IpAddr::V6(address)) => {
            let mask = u128::MAX.checked_shl(128 - bits.min(128)).unwrap_or(0);
            u128::from(network) & mask == u128::from(address) & mask
        }
        _ => false,
    }
}

/// Whether the domain pattern `name` covers the domain `host`: label by
/// label, a leading `*` for one label and `**` for one or more, globs inside
/// the others.
fn covers_domain(name: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let mut patterns: Vec<&str> = name.split('.').collect();
    if patterns.first() == Some(&"**") {
        patterns.remove(0);
        if labels.len() <= patterns.len() {
            return false;
        }
        let rest = &labels[labels.len() - patterns.len()..];
        return labels_match(&patterns, rest);
    }
    labels.len() == patterns.len() && labels_match(&patterns, &labels)
}

/// Whether each label of `labels` matches the pattern of the same rank.
fn labels_match(patterns: &[&str], labels: &[&str]) -> bool {
    patterns
        .iter()
        .zip(labels)
        .all(|(pattern, label)| glob(pattern.as_bytes(), label.as_bytes()))
}

/// Whether `text` matches the glob `pattern`: `*` for any run, `?` for one
/// character, `[…]` and `[!…]` for a class.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| glob(rest, &text[skip..])),
        Some((b'?', rest)) => !text.is_empty() && glob(rest, &text[1..]),
        Some((b'[', rest)) => {
            let Some(end) = rest.iter().position(|&c| c == b']') else {
                return false;
            };
            let (negated, class) = match rest[..end].split_first() {
                Some((b'!', class)) => (true, class),
                _ => (false, &rest[..end]),
            };
            match text.split_first() {
                Some((c, tail)) => class.contains(c) != negated && glob(&rest[end + 1..], tail),
                None => false,
            }
        }
        Some((c, rest)) => text.first() == Some(c) && glob(rest, &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the pattern `text` covers `host` on `port`.
    fn covered(text: &str, host: &str, port: u16) -> bool {
        covers(&HostPattern::parse(text).unwrap(), host, port)
    }

    #[test]
    fn domains_are_covered_label_by_label() {
        assert!(covered("**", "api.example.com", 443));
        assert!(covered("api.example.com", "API.example.com", 443));
        assert!(!covered("example.com", "api.example.com", 443));
        assert!(covered("*.example.com", "api.example.com", 443));
        assert!(!covered("*.example.com", "a.b.example.com", 443));
        assert!(!covered("*.example.com", "example.com", 443));
        assert!(covered("**.example.com", "a.b.example.com", 443));
        assert!(!covered("**.example.com", "example.com", 443));
        assert!(covered("api-?.example.com", "api-1.example.com", 443));
        assert!(covered("api-[12].example.com", "api-2.example.com", 443));
        assert!(!covered("api-[!12].example.com", "api-2.example.com", 443));
        assert!(covered("api-[!12].example.com", "api-3.example.com", 443));
        assert!(!covered("api-?.example.com", "api-.example.com", 443));
        assert!(!covered("api-[12].example.com", "api-.example.com", 443));
        // `HostPattern` refuses a class left open, which matches nothing.
        assert!(!glob(b"api-[12", b"api-1"));
        assert!(!covered("api.example.com", "10.0.0.1", 443));
    }

    #[test]
    fn ports_and_addresses_are_compared() {
        assert!(covered("example.com:443", "example.com", 443));
        assert!(!covered("example.com:443", "example.com", 80));
        assert!(covered(
            "host.docker.internal",
            "host.docker.internal",
            47813
        ));
        assert!(covered("10.0.0.0/8", "10.1.2.3", 443));
        assert!(!covered("10.0.0.0/8", "11.1.2.3", 443));
        assert!(covered("10.1.2.3", "10.1.2.3", 443));
        assert!(covered("0.0.0.0/0", "10.1.2.3", 443));
        assert!(covered("2001:db8::/32", "2001:db8::1", 443));
        assert!(covered("[2001:db8::1]:443", "[2001:db8::1]", 443));
        assert!(!covered("[2001:db8::1]:443", "[2001:db8::1]", 80));
        assert!(!covered("10.0.0.0/8", "2001:db8::1", 443));
        assert!(!covered("10.0.0.0/8", "example.com", 443));
        assert!(!covers_address("10.0.0.0/x", "10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn authorities_are_split_from_their_port() {
        assert_eq!(authority("example.com", 443), ("example.com", 443));
        assert_eq!(authority("example.com:8080", 443), ("example.com", 8080));
        assert_eq!(authority("[2001:db8::1]:80", 443), ("[2001:db8::1]", 80));
        assert_eq!(authority("[2001:db8::1]", 443), ("[2001:db8::1]", 443));
        assert_eq!(
            authority("example.com:http", 443),
            ("example.com:http", 443)
        );
    }
}
