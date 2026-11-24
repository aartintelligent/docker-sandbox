//! Passive route probing (`GET` and `OPTIONS` only).
//!
//! [`run`] requests each known or candidate route with `GET` and then
//! `OPTIONS`, and classifies every answer into a verdict telling whether the
//! route exists, expects other input or another method, or is absent (see
//! [`ProbeResult::verdict`]).
//!
//! Golden rule: NO mutating method is ever sent from this module.
//! `/daemon/reset` exists and wipes everything. It is NOT probed, not even with
//! `GET` or `OPTIONS`, because nothing guarantees that its handler checks the
//! method. See [`NEVER_PROBE`].

use crate::client::{SockClient, preview};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;

/// Destructive routes that are never probed, whatever the wordlist contains.
///
/// Matching uses the path without its query string, ignores case and trailing
/// `/` characters, so entries must be lowercase and have no trailing `/`.
pub(crate) const NEVER_PROBE: &[&str] = &["/daemon/reset", "/daemon/shutdown"];

/// Returns `true` when `route` designates one of the [`NEVER_PROBE`] routes.
///
/// The route is normalized the way the daemon's router sees it before the
/// comparison, so that percent-encoding (`/daemon/%72eset`), dot segments
/// (`/x/../daemon/reset`), repeated slashes, a query or a fragment cannot
/// smuggle a forbidden route past the check.
pub(crate) fn is_forbidden(route: &str) -> bool {
    let normalized = normalize_path(route).to_ascii_lowercase();
    NEVER_PROBE.iter().any(|forbidden| normalized == *forbidden)
}

/// Decodes percent-escapes, drops the query and fragment, and resolves `.`,
/// `..` and empty segments: `/a//b/./../%63` becomes `/a/c`.
fn normalize_path(route: &str) -> String {
    let path = route.split(['?', '#']).next().unwrap_or_default();
    let mut segments: Vec<String> = Vec::new();
    for segment in percent_decode(path).split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other.to_owned()),
        }
    }
    format!("/{}", segments.join("/"))
}

/// Value of an ASCII hexadecimal digit.
fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|digit| u8::try_from(digit).ok())
}

/// Decodes `%XX` escapes; invalid escapes are kept as they are.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
        {
            decoded.push(high * 16 + low);
            i += 3;
            continue;
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Known routes (community SDK, `sbx` ~v0.51) plus candidates to confirm.
///
/// `{sandbox}` is replaced with the sandbox name given to [`run`].
pub(crate) const DEFAULT_ROUTES: &[&str] = &[
    // --- daemon (known)
    "/daemon/health",
    "/daemon/diagnostics",
    // --- daemon (observed through the v0.54 CLI)
    "/daemon/settings",
    "/daemon/disk-usage",
    "/docker/images",
    "/mcp/gateway-mode",
    "/policy/network/setup",
    // --- sandboxes (known / assumed)
    "/sandboxes",
    "/sandbox",
    "/sandbox/{sandbox}",
    "/sandbox/{sandbox}/ports",
    "/sandbox/{sandbox}/files?path=/etc/hostname",
    "/sandbox/{sandbox}/exec",
    "/sandbox/{sandbox}/attach",
    "/sandbox/{sandbox}/logs",
    "/sandbox/{sandbox}/stats",
    "/sandbox/{sandbox}/mcp",
    "/sandbox/{sandbox}/policy",
    "/sandbox/{sandbox}/secrets",
    // --- policy / network
    "/policy",
    "/policy/network",
    "/policy/network/rules?type=all",
    "/policy/profiles",
    "/policy/log",
    "/network/log",
    // --- miscellaneous resources
    "/templates",
    "/template",
    "/secrets",
    "/secret",
    "/settings",
    "/mcp",
    "/mcp/servers",
    "/kits",
    "/skills",
    "/version",
    "/health",
    "/_ping",
    "/info",
    // --- official v1 API (served locally?)
    "/v1/sandboxes",
    "/v1/sandboxes/{sandbox}",
    "/v1/sandboxes/{sandbox}/mcp-gateway",
    "/v1/images",
    "/v1/snapshots",
    "/v1/volumes",
    "/v1/secrets",
    "/v1/policies",
    "/v1/files?path=/",
    "/v1/processes",
    // --- possible self-description and debug endpoints
    "/openapi.json",
    "/openapi.yaml",
    "/swagger.json",
    "/docs",
    "/debug/pprof/",
    "/metrics",
];

/// Outcome of a single probe request.
#[derive(Serialize)]
pub(crate) struct ProbeResult {
    /// Route as listed, before `{sandbox}` substitution.
    pub route: String,
    /// HTTP method used: `GET` or `OPTIONS`.
    pub method: String,
    /// HTTP status code, or `None` when no response was received.
    pub status: Option<u16>,
    /// Classification of the answer: `exists`, `exists_bad_input`,
    /// `auth_required`, `route_absent`, `resource_or_route_absent`,
    /// `exists_other_method`, `server_error`, `other`, or `transport_error`
    /// when no response was received.
    pub verdict: String,
    /// `Content-Type` response header, when present and valid as a string.
    pub content_type: Option<String>,
    /// `Allow` response header, when present and valid as a string.
    pub allow: Option<String>,
    /// Response body truncated to 400 bytes by [`preview`], decoded lossily as
    /// UTF-8.
    pub body_preview: Option<String>,
    /// Transport error, when no response was received.
    pub error: Option<String>,
}

/// Classifies an HTTP answer into a verdict string.
///
/// `body` is only used to disambiguate a `404`.
fn verdict(status: u16, body: &str) -> &'static str {
    match status {
        200..=299 => "exists",
        400 | 422 => "exists_bad_input",
        401 | 403 => "auth_required",
        // A 404 is ambiguous: missing route OR missing resource. The body often
        // settles it: the Go mux's plain-text "404 page not found" means the
        // route itself is absent.
        404 if body.contains("page not found") => "route_absent",
        404 => "resource_or_route_absent",
        405 => "exists_other_method",
        500..=599 => "server_error",
        _ => "other",
    }
}

/// Probes the default routes, plus the routes listed in `wordlist` (one per
/// line), on the socket at `sock`.
///
/// `{sandbox}` in a route is replaced with `sandbox`. Wordlist lines are
/// trimmed, and empty lines and lines starting with `#` are ignored. Duplicate
/// routes are dropped, keeping the first occurrence. Routes matching
/// [`NEVER_PROBE`] are skipped with a message on stderr. Every remaining route
/// is requested with `GET` and then `OPTIONS`, producing one [`ProbeResult`]
/// per request.
///
/// # Errors
///
/// Never fails: request failures are recorded in the results with the
/// `transport_error` verdict.
pub(crate) async fn run(
    sock: &Path,
    sandbox: &str,
    wordlist: Option<&str>,
) -> Result<Vec<ProbeResult>> {
    let client = SockClient::new(sock);
    let mut routes: Vec<String> = DEFAULT_ROUTES.iter().map(ToString::to_string).collect();
    if let Some(extra) = wordlist {
        routes.extend(
            extra
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(String::from),
        );
    }

    // Deduplicate while preserving order.
    let mut seen = std::collections::HashSet::new();
    routes.retain(|r| seen.insert(r.clone()));

    let mut out = vec![];
    for r in routes {
        let path = r.replace("{sandbox}", sandbox);
        // Checked after substitution, on the exact path that would be sent.
        if is_forbidden(&path) {
            eprintln!("[probe] skipped (NEVER_PROBE): {path}");
            continue;
        }
        for method in ["GET", "OPTIONS"] {
            let res = client.request(method, &path).await;
            out.push(match res {
                Ok(resp) => {
                    let body = preview(&resp.body, 400);
                    ProbeResult {
                        route: r.clone(),
                        method: method.into(),
                        status: Some(resp.status.as_u16()),
                        verdict: verdict(resp.status.as_u16(), &body).into(),
                        content_type: resp
                            .headers
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from),
                        allow: resp
                            .headers
                            .get("allow")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from),
                        body_preview: Some(body),
                        error: None,
                    }
                }
                Err(e) => ProbeResult {
                    route: r.clone(),
                    method: method.into(),
                    status: None,
                    verdict: "transport_error".into(),
                    content_type: None,
                    allow: None,
                    body_preview: None,
                    error: Some(format!("{e:#}")),
                },
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_routes_cannot_be_smuggled() {
        for route in [
            "/daemon/reset",
            "/daemon/reset/",
            "/DAEMON/Reset",
            "/daemon/%72eset",
            "/daemon/%72%65%73%65%74?force=true",
            "//daemon//reset",
            "/daemon/./reset",
            "/x/../daemon/reset",
            "/daemon/reset#fragment",
        ] {
            assert!(is_forbidden(route), "{route} must be refused");
        }
        for route in [
            "/daemon/health",
            "/daemon/resets",
            "/daemon/reset-cooldown",
            "/sandbox/reset",
        ] {
            assert!(!is_forbidden(route), "{route} must be allowed");
        }
    }

    #[test]
    fn paths_are_normalized() {
        assert_eq!(normalize_path("/a//b/./../%63"), "/a/c");
        assert_eq!(normalize_path("/../.."), "/");
        assert_eq!(percent_decode("%zz%41"), "%zzA");
    }
}
