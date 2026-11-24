//! Programs of the examples workspace run inside a sandbox.
//!
//! A sandbox runs Linux, whatever the host: a program of this workspace is
//! built as a static binary for the architecture of the sandbox
//! (`cargo build --target <arch>-unknown-linux-musl`, which needs
//! `rustup target add <arch>-unknown-linux-musl`), copied into it with
//! `Sandbox::write_file_with_mode`, then started with an execution. The
//! `mcp-*` examples install their MCP client this way, `shell-ports` and
//! `shell-secrets` the tools of `sandbox-tools`.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::sandbox::Sandbox;
use tokio::process::Command;

/// The manifest of the examples workspace.
const EXAMPLES_MANIFEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../Cargo.toml");

/// The target directory of the examples workspace.
const EXAMPLES_TARGET: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target");

/// Where [`install_tools`] copies `example-sandbox-tools` inside a sandbox.
pub const TOOLS: &str = "/tmp/example-sandbox-tools";

/// Builds the binary of `package` for the platform of `sandbox` and copies it
/// to `path` in the sandbox, executable.
///
/// # Errors
///
/// Fails when the architecture of the sandbox cannot be read, when the
/// program cannot be built (the musl target of that architecture is
/// missing), or when it cannot be copied.
pub async fn install(sandbox: &Sandbox, package: &str, path: &str) -> Result<()> {
    let arch = sandbox
        .exec(&ExecSpec::new(["uname", "-m"]))
        .await?
        .check()?
        .stdout
        .trim()
        .to_owned();
    let binary = build(package, &arch).await?;
    let bytes = tokio::fs::read(&binary)
        .await
        .with_context(|| format!("reading {}", binary.display()))?;
    sandbox.write_file_with_mode(path, bytes, 0o755).await?;
    eprintln!("{package} copied into the sandbox ({arch})");
    Ok(())
}

/// Installs `example-sandbox-tools` at [`TOOLS`] in `sandbox`: a static web
/// server (`serve <port> <directory>`) and an HTTP client that goes through
/// the proxy of the sandbox (`get <url> [<header> <value>]...`).
///
/// # Errors
///
/// Fails as [`install`] does.
pub async fn install_tools(sandbox: &Sandbox) -> Result<()> {
    install(sandbox, "example-sandbox-tools", TOOLS).await
}

/// Builds `package` for `<arch>-unknown-linux-musl` and returns its binary.
async fn build(package: &str, arch: &str) -> Result<PathBuf> {
    let target = format!("{arch}-unknown-linux-musl");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    eprintln!("building {package} for {target}");
    let status = Command::new(cargo)
        .args(["build", "--release", "--quiet"])
        .args(["--manifest-path", EXAMPLES_MANIFEST])
        .args(["-p", package, "--target", &target])
        .stdin(Stdio::null())
        .status()
        .await
        .context("running cargo")?;
    if !status.success() {
        bail!("cannot build {package}: is the target installed? rustup target add {target}");
    }
    Ok(PathBuf::from(EXAMPLES_TARGET)
        .join(target)
        .join("release")
        .join(package))
}
