//! What `sbx run` passes from the host to an agent session.
//!
//! `sandboxd` starts the agent with the environment of the session request
//! only, and the CLI fills it from the host (`run --name` in the `sessions`
//! suite of the audit): the terminal type, so that the agent draws its
//! interface with the right capabilities and colors; the time zone, which is
//! UTC otherwise; and the desktop session of the host as `SBX_HOST_*`
//! variables, through which the sandbox reaches it (opening a URL in the host
//! browser). The SSH agent of the host is forwarded as well, unless the
//! `ssh.agentForwardingEnabled` setting turns it off.

use docker_sandbox::Client;
use docker_sandbox::exec::{AgentSessionSpec, TerminalSize};
use serde_json::Value;

/// Variables of the terminal, passed as they are.
const TERMINAL: &[&str] = &["TERM", "COLORTERM"];

/// Variables of the desktop session, passed as `SBX_HOST_<NAME>`.
const DESKTOP: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
];

/// Zone file of the host, read when `TZ` is not set.
const LOCALTIME: &str = "/etc/localtime";

/// The agent session `sbx run` would open from this terminal, of `size`.
pub async fn agent_session(client: &Client, size: TerminalSize) -> AgentSessionSpec {
    let spec = AgentSessionSpec::default()
        .with_tty(size)
        .with_envs(session_env());
    match ssh_agent_socket(client).await {
        Some(socket) => spec.with_ssh_agent_socket(socket),
        None => spec,
    }
}

/// `TERM`, `COLORTERM` and `TZ`, and the desktop session as `SBX_HOST_*`
/// variables, as far as the host has them.
#[must_use]
pub fn session_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = TERMINAL
        .iter()
        .filter_map(|name| var(name).map(|value| ((*name).to_owned(), value)))
        .collect();
    if let Some(zone) = time_zone() {
        env.push(("TZ".to_owned(), zone));
    }
    env.extend(
        DESKTOP
            .iter()
            .filter_map(|name| var(name).map(|value| (format!("SBX_HOST_{name}"), value))),
    );
    env
}

/// The SSH agent socket `sbx` forwards: none when the
/// `ssh.agentForwardingEnabled` setting is off, else the `ssh.agentSocketPath`
/// setting, else `$SSH_AUTH_SOCK`.
pub async fn ssh_agent_socket(client: &Client) -> Option<String> {
    let daemon = client.daemon();
    if let Ok(enabled) = daemon.setting("ssh.agentForwardingEnabled").await
        && enabled.value == Value::Bool(false)
    {
        return None;
    }
    if let Ok(path) = daemon.setting("ssh.agentSocketPath").await
        && let Some(path) = path.value.as_str().filter(|p| !p.is_empty())
    {
        return Some(path.to_owned());
    }
    var("SSH_AUTH_SOCK")
}

/// The time zone of the host as a POSIX `TZ` rule: `$TZ`, else the rule that
/// ends the zone file of the host (`TZif` version 2 and later), daylight saving
/// time included.
fn time_zone() -> Option<String> {
    var("TZ").or_else(|| {
        let zone_file = std::fs::read(LOCALTIME).ok()?;
        tz_rule(&zone_file)
    })
}

/// The POSIX `TZ` rule of a `TZif` file: its last line, from version 2 on.
fn tz_rule(zone_file: &[u8]) -> Option<String> {
    let version = *zone_file.strip_prefix(b"TZif")?.first()?;
    if version < b'2' {
        return None;
    }
    let text = zone_file.strip_suffix(b"\n")?;
    let start = text.iter().rposition(|&b| b == b'\n')? + 1;
    let rule = std::str::from_utf8(&text[start..]).ok()?;
    (!rule.is_empty()).then(|| rule.to_owned())
}

/// The value of the environment variable `name`, when set and not empty.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_zone_rule_is_the_last_line_of_a_version_2_file() {
        let mut zone_file = b"TZif2".to_vec();
        zone_file.extend_from_slice(&[0; 40]);
        zone_file.extend_from_slice(b"\nCET-1CEST,M3.5.0,M10.5.0/3\n");
        assert_eq!(
            tz_rule(&zone_file).as_deref(),
            Some("CET-1CEST,M3.5.0,M10.5.0/3")
        );
        assert_eq!(tz_rule(b"TZif\0\nUTC0\n"), None, "version 1 has no rule");
        assert_eq!(tz_rule(b"not a zone file\n"), None);
    }
}
