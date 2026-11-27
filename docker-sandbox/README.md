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
kits forged through the socket alone or compiled once with the CLI, keep them
running, run commands and agent sessions, move files in and out, publish
ports, give agents their credentials and scope network access.

```rust,no_run
use std::time::Duration;

use docker_sandbox::prelude::*;

#[tokio::main]
async fn main() -> docker_sandbox::Result<()> {
    let client = Client::connect().await?;

    // A sandbox of the claude kit, from a package forged through the socket
    // (below) or compiled once with the CLI. The daemon gives the agent the
    // credential it holds for the kit, a Claude subscription for instance.
    let package = KitPackage::open("/var/lib/kits/claude")?;
    let created = package.create(&client, "job-42").await?;

    // Handles own a clone of the client: store them, move them into tasks.
    let sandbox = client.sandbox(created.name);
    let _running = sandbox.ensure_running().await?;
    sandbox.upload_dir("./repo", "/home/agent").await?;

    // Claude Code, unattended, on a copy of the repository: the sandbox is
    // the boundary of what it may do.
    let output = sandbox
        .exec(
            &ExecSpec::new([
                "claude",
                "--dangerously-skip-permissions",
                "-p",
                "Run the tests, fix what fails, and summarize the fix.",
            ])
            .with_working_dir("/home/agent/repo")
            .with_timeout(Duration::from_secs(900)),
        )
        .await?
        .check()?;
    println!("{}", output.stdout);

    sandbox.force_delete().await
}
```

With the `kit-forge` feature, that package needs no CLI at all: `KitForge`
rebuilds the published kit from its image through the socket, with its kit
arguments and mixins of your own, in the form the CLI would give it, so that
the daemon treats the sandbox as one the CLI created, the credential of its
agent included. To give the agent an API key of your own instead, use a
`ProxiedKey`: the sandbox only holds a placeholder.

```rust,ignore
use docker_sandbox::kit::KitForge;

let package = KitForge::new(client.clone())
    .with_base("docker.io/sbx/claude:latest")
    .with_arg("model", "claude-haiku-5-5")
    .forge("/var/lib/kits/claude")
    .await?;
```

> This project is not affiliated with Docker, Inc. The `sandboxd` socket API is
> private and changes with every `sbx` release: the audit tool of this
> repository keeps a map of it per version, and the library is validated
> against the version recorded in `audit/snapshots/`.

## What the library covers

| Module | Covers |
|---|---|
| `sandbox` | Listing, creation with its progress stream, start, stop, deletion, saving as a template, sessions that keep a sandbox running, ports, host paths, credentials and API keys given through the proxy alone (`ProxiedKey`), files and directories, SSH tunnels |
| `exec` | Commands run to completion, interactive executions and agent sessions over upgraded connections, terminal resizing |
| `policy` | Network rules (global or per sandbox), checks of a target, the activity log of the proxy |
| `kit` | Kit packages replayed through the socket alone: forged without the CLI, a published base kit rebuilt with its arguments and mixins described in JSON or in Rust (feature `kit-forge`), or compiled once with the CLI (feature `kit-compiler`); sandboxes created from them, kits added to a running sandbox in place or by recreating its container |
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
| `kit-forge` | `KitForge`, `KitDescriptor`, `LifecycleStep` and `Mixin`: forging packages without the CLI, through the socket alone: the base kit rebuilt from its published image with its arguments, or taken from a compiled package, and mixins described in JSON or in Rust |
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
- **Agents get their credential from the daemon.** The kit of an agent declares
  its credential, and the daemon gives the sandbox the one it holds for that
  service, a subscription for instance, when it authorizes it: for a published
  kit whose package is the one the CLI resolves, forged or compiled, or with a
  bindings file. `Created::withheld` names what it withholds. To give an agent
  a key of your own, use a `ProxiedKey`: the sandbox only holds a placeholder,
  and the proxy writes the key into the requests to the hosts of the key.

The [examples](../examples/README.md) of the repository are complete programs,
one crate each: the basics, shell sandboxes (commands, files, network policy,
ports, interactive shell, events), kits forged without the CLI, and every
coding agent `sbx` knows, its kit forged through the socket, with and without
an MCP server.

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
