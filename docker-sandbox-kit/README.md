# docker-sandbox-kit

[![Crates.io](https://img.shields.io/crates/v/docker-sandbox-kit.svg)](https://crates.io/crates/docker-sandbox-kit)
[![docs.rs](https://img.shields.io/docsrs/docker-sandbox-kit)](https://docs.rs/docker-sandbox-kit)
[![CI](https://github.com/aartintelligent/docker-sandbox/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/aartintelligent/docker-sandbox/actions/workflows/ci.yml?query=branch%3Amain)
[![MSRV](https://img.shields.io/crates/msrv/docker-sandbox-kit)](https://crates.io/crates/docker-sandbox-kit)
[![License](https://img.shields.io/crates/l/docker-sandbox-kit.svg)](https://github.com/aartintelligent/docker-sandbox/blob/main/docker-sandbox-kit/README.md#license)

Kits of [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) (`sbx`)
composed in Rust, for [`docker-sandbox`](../docker-sandbox/README.md), the
typed client of the `sandboxd` socket: what the `sbx` CLI does before a
sandbox can be created, done by a program, without the CLI.

## Why a kit package

`sandboxd` cannot resolve kits by itself. `POST /sandbox` takes the kit
artifacts the CLI compiles from the kit descriptors, and the images of local
kit sources must already be loaded. A *kit package* holds that work once:

```text
<package>/kit.json            what made it: forged by this crate, or compiled by sbx
<package>/create-body.json    the creation request, its kit artifacts compiled
<package>/images/NN.oci.tar   the images of the local mixins, loaded before creation
```

`docker-sandbox` replays a package through the socket, as many times as
needed (`KitPackage::open`, `KitPackage::create`). This crate writes it, in one
of two ways: forged through the socket alone (`KitForge`), or compiled once by
the CLI (`KitCompiler`).

## Forging a kit

A kit is a base kit and mixins. The base is a published kit, rebuilt from its
image through the socket, with the arguments it declares; a mixin adds install
and startup commands and network rules, described in Rust or read from its
directory.

```rust,no_run
use docker_sandbox::Client;
use docker_sandbox_kit::{KitDescriptor, KitForge, LifecycleStep, Mixin};

#[tokio::main]
async fn main() -> docker_sandbox_kit::Result<()> {
    let client = Client::connect().await?;

    // A mixin described in Rust: what the project needs on top of the kit.
    let tools = KitDescriptor::mixin("Project tools")
        .with_description("The tools of the project, installed once")
        .with_install(
            LifecycleStep::new("pip install --user ruff")
                .as_user("agent")
                .with_description("Installs ruff"),
        )
        .with_startup(LifecycleStep::new("python3 -m http.server 8000").in_background())
        .with_network_deny(["*.pastebin.com"]);

    let package = KitForge::new(client.clone())
        // The published claude kit, rebuilt from its image, with two of the
        // arguments it declares, checked against what it declares.
        .with_base("docker.io/sbx/claude:latest")
        .with_arg("model", "claude-haiku-5-5")
        .with_arg("reasoningEffort", "low")
        .with_mixin(Mixin::new("/kits/project-tools", tools)?)
        // A mixin of its own: `./kits/acme/spec.yaml`, in the JSON subset of
        // YAML.
        .with_mixin(Mixin::read("./kits/acme")?)
        .forge("/var/lib/kits/claude+tools")
        .await?;

    // From here on, docker-sandbox replays the package through the socket.
    let created = package.create(&client, "job-42").await?;
    println!("{} created; withheld credentials: {:?}", created.name, created.withheld);
    Ok(())
}
```

The artifacts are written as the CLI writes them, down to the order of their
capabilities, which decides whether the daemon gives the agent the credential
of its kit, a Claude subscription for instance. The audit of the repository
compares them field by field with those of the CLI at each `sbx` version.

## Checked before anything is written

`KitForge` refuses a composition the daemon would reject or half apply, with an
`Error::InvalidKit` or an `Error::Forge` that names the kit and the reason:

- **a mixin**: not a v3 mixin, a capability other than `lifecycle` and
  `network-policy`, a field the crate does not know, a step without a command,
  a variable name that is not one, a background command outside startup, an
  empty host, kit arguments it would never substitute;
- **the base kit**: not a v3 workload kit, an argument it does not declare, a
  required one missing, a value outside its choices or its pattern, a
  capability whose place among the others the crate does not know, a pinned
  reference whose image holds another digest;
- **the composition**: two kits of the same name, or a kit policy of the
  daemon that refuses it (`kit.requireSignature`, `kit.allowLocalKits`,
  `kit.allowedSources`), read from its settings.

## Compiling with the CLI

When a kit needs what forging does not cover, `KitCompiler` (feature
`compiler`) runs `sbx create` once behind a proxy that captures what the CLI
sends, and writes the same package. The CLI is needed then, once; the package
is tied to its version.

```rust,no_run
# #[cfg(feature = "compiler")]
# async fn run() -> docker_sandbox_kit::Result<()> {
use docker_sandbox::Client;
use docker_sandbox_kit::{CompileJob, KitCompiler};

let client = Client::connect().await?;
let job = CompileJob::new("shell", "/var/lib/kits/shell+acme").with_mixin("./kits/acme");
let package = KitCompiler::new("sbx", client.clone(), "/tmp/kc").compile(&job).await?;
# let _ = package;
# Ok(())
# }
```

## Reusing a package

A package stays on disk: `KitPackage::open` takes it again for the next
sandboxes, without forging it again. `KitForge::with_base_package` takes its
base kit, forged or compiled, for another composition, without pulling the
image again. A forged package is tied to the API of the daemon it was forged
for, a compiled one to the `sbx` version that compiled it.

The [`shell-forge`](../examples/shell-forge/src/main.rs) and
[`agent-kit-forge`](../examples/agent-kit-forge/src/main.rs) examples of the
repository run all of this against the real daemon.

## Features, version and errors

| Feature | Adds |
|---|---|
| `forge` (default) | `KitForge`, `KitDescriptor`, `LifecycleStep` and `Mixin` |
| `compiler` | `KitCompiler` and `CompileJob`; needs the `sbx` CLI at run time |
| `live` | Nothing to the library: runs the integration tests against the real daemon |

The crate shares the version of `docker-sandbox`: use the two at the same
version. Its `Error` keeps the errors of the client as they are, in
`Error::Client`, so that `?` takes both in a function returning
`docker_sandbox_kit::Result`.

## Requirements

- Rust 1.88 or later.
- Linux or macOS: the daemon is reached through a Unix socket.
- Run as the user owning the daemon. The socket has no authentication: whoever
  can open it controls every sandbox of that user.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
