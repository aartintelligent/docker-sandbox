# docker-sandbox-kit

[![Crates.io](https://img.shields.io/crates/v/docker-sandbox-kit.svg)](https://crates.io/crates/docker-sandbox-kit)
[![docs.rs](https://img.shields.io/docsrs/docker-sandbox-kit)](https://docs.rs/docker-sandbox-kit)
[![CI](https://github.com/aartintelligent/docker-sandbox/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/aartintelligent/docker-sandbox/actions/workflows/ci.yml?query=branch%3Amain)
[![MSRV](https://img.shields.io/crates/msrv/docker-sandbox-kit)](https://crates.io/crates/docker-sandbox-kit)
[![License](https://img.shields.io/crates/l/docker-sandbox-kit.svg)](https://github.com/aartintelligent/docker-sandbox/blob/main/docker-sandbox-kit/README.md#license)

The kit packages of [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/)
(`sbx`), for [`docker-sandbox`](../docker-sandbox/README.md), the typed Rust
client of the `sandboxd` socket.

`sandboxd` cannot resolve kits by itself: `POST /sandbox` only takes kit
artifacts already compiled, and the images of local kit sources already
loaded. The `sbx` CLI does that work on the client side. This crate does it for
a program, and writes a kit package that `docker-sandbox` replays through the
socket: forged through the socket alone, or compiled once with the CLI.

```rust,no_run
use std::time::Duration;

use docker_sandbox::prelude::*;
use docker_sandbox_kit::KitForge;

#[tokio::main]
async fn main() -> docker_sandbox_kit::Result<()> {
    let client = Client::connect().await?;

    // The claude kit, rebuilt from its published image through the socket:
    // no `sbx` CLI, not even once. The daemon treats the sandbox as one the
    // CLI created, and gives the agent the credential it holds for the kit,
    // a Claude subscription for instance.
    let package = KitForge::new(client.clone())
        .with_base("docker.io/sbx/claude:latest")
        .with_arg("model", "claude-haiku-5-5")
        .forge("/var/lib/kits/claude")
        .await?;
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

    sandbox.force_delete().await?;
    Ok(())
}
```

The package stays on disk: `KitPackage::open` takes it again for the next
sandboxes, without forging it again.

## What the crate covers

| Type | Covers |
|---|---|
| `KitForge` | Forging a kit package through the socket alone: the base kit rebuilt from its published image with its arguments, or taken from a compiled package, plus mixins; the kit policy of the daemon checked first |
| `KitDescriptor`, `LifecycleStep`, `Mixin` | Mixins described in Rust or read from their directory (`spec.yaml`, in the JSON subset of YAML), checked before anything is written |
| `KitCompiler`, `CompileJob` | Compiling a kit composition once with the `sbx` CLI, behind a proxy that captures what the CLI sends |
| `Error` | What a kit gets wrong, and the errors of the client of `docker-sandbox`, kept as they are |

A forged package is tied to the API of the daemon it was forged for, a
compiled one to the `sbx` version that compiled it. The version of this crate
follows the kit compiler of the CLI, whose output it writes, rather than the
API of the daemon, which `docker-sandbox` follows.

| Feature | Adds |
|---|---|
| `forge` (default) | `KitForge`, `KitDescriptor`, `LifecycleStep` and `Mixin` |
| `compiler` | `KitCompiler` and `CompileJob`; needs the `sbx` CLI at run time |
| `live` | Nothing to the library: runs the integration tests against the real daemon |

## Requirements

- Rust 1.88 or later.
- Linux or macOS: the daemon is reached through a Unix socket.
- Run as the user owning the daemon. The socket has no authentication: whoever
  can open it controls every sandbox of that user.

## Repository

This crate is part of the [docker-sandbox](../README.md) repository, next to
the client it writes packages for, its examples, and the audit tool that keeps
the map of the daemon API and compares forged packages with those of the CLI.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
