//! Location of the daemon socket.
//!
//! The `sbx` CLI finds `sandboxd` through `DOCKER_SANDBOXES_API`, then under
//! the XDG state directory of the user. The same resolution is applied here so
//! that a program and the CLI always talk to the same daemon.

use std::ffi::OsString;
use std::path::PathBuf;

/// Environment variable overriding the socket path, honoured by the `sbx` CLI.
///
/// The CLI ignores it when run as root.
pub const SOCKET_ENV: &str = "DOCKER_SANDBOXES_API";

/// Socket path relative to the XDG state directory.
const SOCKET_RELATIVE_PATH: &str = "sandboxes/sandboxes/sandboxd/sandboxd.sock";

/// Socket path used by the `sbx` CLI for the current user.
///
/// Resolution order: `$DOCKER_SANDBOXES_API`, then
/// `$XDG_STATE_HOME/sandboxes/sandboxes/sandboxd/sandboxd.sock`, then the same
/// path under `$HOME/.local/state`. Returns `None` when none of these variables
/// is set.
#[must_use]
pub fn default_socket_path() -> Option<PathBuf> {
    socket_path_from(std::env::var_os)
}

/// Socket path resolved from the environment variables `var` reads, kept apart
/// from the process environment so that the resolution order can be tested.
fn socket_path_from(var: impl Fn(&'static str) -> Option<OsString>) -> Option<PathBuf> {
    let non_empty = |name| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);

    if let Some(path) = non_empty(SOCKET_ENV) {
        return Some(path);
    }
    let state_dir = non_empty("XDG_STATE_HOME")
        .or_else(|| non_empty("HOME").map(|home| home.join(".local/state")))?;
    Some(state_dir.join(SOCKET_RELATIVE_PATH))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// The socket path resolved with only `vars` set.
    fn resolve(vars: &[(&'static str, &str)]) -> Option<PathBuf> {
        let vars: HashMap<&str, &str> = vars.iter().copied().collect();
        socket_path_from(|name| vars.get(name).map(OsString::from))
    }

    #[test]
    fn the_cli_variable_wins_then_the_state_directory_then_home() {
        let state = PathBuf::from("/var/state").join(SOCKET_RELATIVE_PATH);
        let home = PathBuf::from("/home/agent/.local/state").join(SOCKET_RELATIVE_PATH);
        assert_eq!(
            resolve(&[
                (SOCKET_ENV, "/run/sandboxd.sock"),
                ("XDG_STATE_HOME", "/var/state"),
                ("HOME", "/home/agent"),
            ]),
            Some(PathBuf::from("/run/sandboxd.sock"))
        );
        assert_eq!(
            resolve(&[("XDG_STATE_HOME", "/var/state"), ("HOME", "/home/agent")]),
            Some(state)
        );
        assert_eq!(resolve(&[("HOME", "/home/agent")]), Some(home.clone()));
        assert_eq!(
            resolve(&[
                (SOCKET_ENV, ""),
                ("XDG_STATE_HOME", ""),
                ("HOME", "/home/agent")
            ]),
            Some(home),
            "empty variables are unset"
        );
        assert_eq!(resolve(&[]), None);
    }
}
