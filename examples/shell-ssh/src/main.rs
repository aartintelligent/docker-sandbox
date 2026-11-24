//! SSH into a shell sandbox through the socket: the tunnel of the library
//! (`Sandbox::ssh_tunnel`), relayed to the SSH client of the system.
//!
//! The daemon exposes SSH as an API: `POST /sandbox/{name}/ssh` turns the
//! connection into a tunnel to the SSH server of the sandbox, which greets
//! with `SSH-2.0-Go`. No network port is published and no client key is
//! needed: the socket is the boundary, and the server accepts the `none`
//! method for any user, `root` included. A program that relays this tunnel
//! further must therefore guard it as it guards the socket.
//!
//! `sbx setup ssh` writes a `ProxyCommand` running `sbx ssh proxy` into the
//! SSH configuration of the user. This example writes nothing there: it runs
//! `ssh` with `-F /dev/null`, a temporary `known_hosts`, no key and no agent,
//! and a `ProxyCommand` that runs the example binary again as
//! `ssh-proxy <sandbox>`, which relays standard input and output to the
//! tunnel.
//!
//! ```sh
//! just example shell-ssh
//! ```

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use docker_sandbox::Client;
use example_common::{connect, kit, sandbox};
use tokio::io::{AsyncBufReadExt, BufReader};

/// First argument with which `ssh` runs the example binary as its proxy.
const RELAY: &str = "ssh-proxy";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [command, name] = args.as_slice()
        && command == RELAY
    {
        return relay(name).await;
    }

    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("ssh");
    sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        let mut banner = String::new();
        BufReader::new(sandbox.ssh_tunnel().await?)
            .read_line(&mut banner)
            .await?;
        ensure!(
            banner.starts_with("SSH-2.0-"),
            "not an SSH server: {banner:?}"
        );
        println!("the tunnel reaches an SSH server: {}", banner.trim_end());

        let work = std::env::temp_dir().join(format!("example-ssh-{}", std::process::id()));
        std::fs::create_dir_all(&work)?;
        let outcome = async {
            for user in ["agent", "root"] {
                let answer = ssh(&work, sandbox.name(), user, "id -un; pwd").await?;
                println!(
                    "ssh {user}@{}: {}",
                    sandbox.name(),
                    answer.lines().collect::<Vec<_>>().join(", ")
                );
            }
            anyhow::Ok(())
        }
        .await;
        let _ = std::fs::remove_dir_all(&work);
        outcome
    })
    .await
}

/// Relays standard input and output to the SSH tunnel of the sandbox `name`:
/// the `ProxyCommand` side of `ssh`.
async fn relay(name: &str) -> Result<()> {
    let client = Client::connect().await?;
    let mut tunnel = client.sandbox(name).ssh_tunnel().await?;
    let mut local = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    // The connection ends when either side closes it.
    let _ = tokio::io::copy_bidirectional(&mut tunnel, &mut local).await;
    Ok(())
}

/// Runs `command` as `user` in the sandbox `name` with the SSH client of the
/// system, through the relay, with no configuration, key or agent of the
/// user, and returns its standard output.
async fn ssh(work: &Path, name: &str, user: &str, command: &str) -> Result<String> {
    let exe = std::env::current_exe().context("finding the example binary")?;
    let output = tokio::process::Command::new("ssh")
        .args(["-F", "/dev/null"])
        .arg("-o")
        .arg(format!("ProxyCommand='{}' {RELAY} {name}", exe.display()))
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile={}",
            work.join("known_hosts").display()
        ))
        .args(["-o", "GlobalKnownHostsFile=/dev/null"])
        // The host key is new at each run: accept it, and keep it for this
        // run only.
        .args(["-o", "StrictHostKeyChecking=accept-new"])
        .args(["-o", "BatchMode=yes", "-o", "IdentityAgent=none"])
        .args(["-o", "IdentitiesOnly=yes", "-o", "IdentityFile=/dev/null"])
        .args(["-o", "ConnectTimeout=20", "-o", "LogLevel=ERROR"])
        .arg(format!("{user}@{name}"))
        .arg(command)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("running ssh: is an SSH client installed?")?;
    if !output.status.success() {
        bail!(
            "ssh {user}@{name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}
