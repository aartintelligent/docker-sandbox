# docker-sandbox

[![Crates.io](https://img.shields.io/crates/v/docker-sandbox.svg)](https://crates.io/crates/docker-sandbox)
[![docs.rs](https://img.shields.io/docsrs/docker-sandbox)](https://docs.rs/docker-sandbox)
[![CI](https://github.com/aartintelligent/docker-sandbox/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/aartintelligent/docker-sandbox/actions/workflows/ci.yml?query=branch%3Amain)
[![codecov](https://codecov.io/gh/aartintelligent/docker-sandbox/branch/main/graph/badge.svg)](https://codecov.io/gh/aartintelligent/docker-sandbox)
[![Security audit](https://github.com/aartintelligent/docker-sandbox/actions/workflows/audit.yml/badge.svg?branch=main)](https://github.com/aartintelligent/docker-sandbox/actions/workflows/audit.yml?query=branch%3Amain)
[![MSRV](https://img.shields.io/crates/msrv/docker-sandbox)](https://crates.io/crates/docker-sandbox)
[![License](https://img.shields.io/crates/l/docker-sandbox.svg)](https://github.com/aartintelligent/docker-sandbox/blob/main/docker-sandbox/README.md#license)

A typed async Rust client for the `sandboxd` socket of
[Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) (`sbx`). It drives the
daemon directly, without going through the `sbx` CLI: create sandboxes from
compiled kits, keep them running, run commands and agent sessions, move files
in and out, publish ports, push credentials and scope network access.

```rust,no_run
use std::time::Duration;

use docker_sandbox::prelude::*;

#[tokio::main]
async fn main() -> docker_sandbox::Result<()> {
    let client = Client::connect().await?;

    // A sandbox created from a kit package compiled once with the CLI.
    let package = KitPackage::open("/var/lib/kits/shell")?;
    let created = package.create(&client, "job-42").await?;

    // Handles own a clone of the client: store them, move them into tasks.
    let sandbox = client.sandbox(created.name);
    let _running = sandbox.ensure_running().await?;
    sandbox.allow_hosts(["api.github.com"]).await?;
    sandbox.upload_dir("./repo", "/home/agent").await?;

    let output = sandbox
        .exec(
            &ExecSpec::shell("cd /home/agent/repo && make test")
                .with_timeout(Duration::from_secs(600)),
        )
        .await?
        .check()?;
    println!("{}", output.stdout);

    sandbox.force_delete().await
}
```

> This project is not affiliated with Docker, Inc. The `sandboxd` socket API is
> private and changes with every `sbx` release: the audit tool of this
> repository keeps a map of it per version, and the library is validated
> against the version recorded in `audit/snapshots/`.

## What the library covers

| Module | Covers |
|---|---|
| `sandbox` | Listing, creation with its progress stream, start, stop, deletion, saving as a template, sessions that keep a sandbox running, ports, host paths, credentials, files and directories, SSH tunnels |
| `exec` | Commands run to completion, interactive executions and agent sessions over upgraded connections, terminal resizing |
| `policy` | Network rules (global or per sandbox), checks of a target, the activity log of the proxy |
| `kit` | Kit packages compiled once with the CLI then replayed through the socket alone: creation, kits added to a running sandbox in place or by recreating its container, their compilation (feature `kit-compiler`), and mixins forged without the CLI (feature `kit-forge`) |
| `image` | Listing, inspection, pulls, loading, export, removal |
| `event` | The daemon event stream, with or without the initial replay |
| `mcp` | The MCP gateway of a sandbox, the servers loaded into it, and policy checks of a server before it is registered |
| `prompt` | Approval prompts raised by the sandboxes |
| `daemon` | Health, settings, log levels, disk usage, diagnostics |

Every route of the daemon is exposed except, deliberately, those that wipe or
stop it (`/daemon/reset`, `/daemon/shutdown`), change its global network setup,
reload the OAuth credentials stored by the CLI, or run VM debug actions whose
allow-lists are unknown.

| Feature | Adds |
|---|---|
| `kit-compiler` | `KitCompiler` and `CompileJob`: compiling kit compositions into packages with the `sbx` CLI |
| `kit-forge` | `KitForge`, `KitDescriptor`, `LifecycleStep` and `Mixin`: forging packages without the CLI, mixins described in JSON or in Rust on the base kit of a compiled package |
| `live` | Nothing to the library: runs the integration tests against the real daemon |
| `secrecy` | `From<secrecy::SecretString>` for `Secret`: a secret held as a `SecretString` is passed on without being exposed |

## Building a runner on it

- **Handles are owned.** `Client`, `Sandbox` and the other handles are cheap to
  clone, `Send + Sync + 'static`: keep one per sandbox in your state and move
  clones into tasks. Every operation is a `Send` future.
- **Keep sandboxes running explicitly.** The daemon stops a sandbox about 30
  seconds after its last session closes. `Sandbox::ensure_running` returns a
  `SessionGuard`; clones share one session, which closes with the last clone.
- **Errors say what failed.** Daemon errors carry the operation, the status and
  the daemon code; `Error::is_not_found`, `is_conflict`, `is_transient` and
  `is_timeout` classify them, and `ExecOutput::check` turns a non-zero exit code
  into `Error::CommandFailed` with the outputs.
- **Deadlines are explicit.** The client has a regular and a long-operation
  timeout; `ExecSpec::with_timeout` gives a command its own.
- **Streams need no extra crate.** Creation progress, pulls, events, prompts and
  interactive output implement `futures_core::Stream` and an inherent `next()`.

The [examples](../examples/README.md) of the repository are complete programs,
one crate each: the basics, shell sandboxes (commands, files, network policy,
ports, interactive shell, events) and every coding agent `sbx` knows, with and
without an MCP server.

## Requirements

- Rust 1.88 or later (`rust-toolchain.toml` pins the development toolchain).
- Linux or macOS: the daemon is reached through a Unix socket.
- Run as the user owning the daemon. The socket has no authentication: whoever
  can open it controls every sandbox of that user.

## Repository

This crate is the library of the [docker-sandbox](../README.md) repository,
next to its examples and the audit tool that keeps the map of the daemon API.
The audit's [README](../audit/README.md) is the workflow to follow when `sbx`
changes.

## Documentation

- The API reference: `cargo doc --features kit-compiler,kit-forge,secrecy --no-deps --open`.
- The measured contract of the audited version:
  `audit/snapshots/<version>/api/` (`API.md`, `openapi.json`).
- [`CONTRIBUTING.md`](../CONTRIBUTING.md): tooling, quality gates, commits.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
