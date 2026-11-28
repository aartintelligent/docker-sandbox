# docker-sandbox-blueprint

[![Crates.io](https://img.shields.io/crates/v/docker-sandbox-blueprint.svg)](https://crates.io/crates/docker-sandbox-blueprint)
[![docs.rs](https://img.shields.io/docsrs/docker-sandbox-blueprint)](https://docs.rs/docker-sandbox-blueprint)
[![CI](https://github.com/aartintelligent/docker-sandbox/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/aartintelligent/docker-sandbox/actions/workflows/ci.yml?query=branch%3Amain)
[![MSRV](https://img.shields.io/crates/msrv/docker-sandbox-blueprint)](https://crates.io/crates/docker-sandbox-blueprint)
[![License](https://img.shields.io/crates/l/docker-sandbox-blueprint.svg)](https://github.com/aartintelligent/docker-sandbox/blob/main/docker-sandbox-blueprint/README.md#license)

A whole sandbox of [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/)
(`sbx`) described as data, for
[`docker-sandbox`](../docker-sandbox/README.md), the typed client of the
`sandboxd` socket, and [`docker-sandbox-kit`](../docker-sandbox-kit/README.md),
which forges its kits without the `sbx` CLI.

## A sandbox as data

A `SandboxSpec` holds everything a sandbox receives but its name and the
values of its keys, given at launch: one spec serves every sandbox of a job,
and holds nothing secret. It is written in Rust with chained methods, and the
prelude imports everything it takes, the types of the two other crates
included:

```rust
use docker_sandbox_blueprint::prelude::*;

let spec = SandboxSpec::agent(Agent::Claude)
    .with_arg("model", "claude-haiku-5-5")
    .with_mixin_dir("/srv/kits/project-tools")
    .with_workspace("/srv/project")
    .with_mount(Workspace::new("/srv/docs").read_only().at("/docs"))
    .with_network_deny(["*.pastebin.com"])
    .with_key(KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic"))
    .with_mcp(McpServer::host("tools", 47813))
    .with_env("LOG", "debug")
    .with_cpus(2)
    .with_memory("4g");
```

Each method becomes the call of a brick the two other crates publish; the
crate adds no call to the daemon of its own:

| Method | Becomes |
|---|---|
| `agent`, `base` or `package` | `KitForge::agent`, `KitForge::with_base`, or a compiled package, as it is or with `KitForge::with_base_package` |
| `with_arg` | `KitForge::with_arg` |
| `with_mixin`, `with_mixin_dir` | `Mixin::new`, `Mixin::read` |
| `with_workspace`, `with_cloned_workspace` | `with_workspace`, `with_cloned_workspace` |
| `with_mount` | `with_additional_workspace` |
| `with_network_allow`, `with_network_deny` | a generated `network-policy@1` mixin |
| `with_key` | `ProxiedKey::for_service` or `ProxiedKey::new`; an `optional()` key is left out without a value, or when no kit declares its service |
| `with_mcp` | `Mixin::mcp` |
| `with_env` | `with_envs`, in clear in the request: never a secret |
| `with_cpus`, `with_memory` | `with_cpus`, `with_memory` |
| `keep_on_failure` | whether a failed launch keeps the sandbox |

The network rules are those of a mixin: they apply from the creation of the
sandbox, its install commands included, and go with it. A refusal wins over
any allowance, and the global policy of the daemon allows every host by
default: an allowance restricts nothing.

## Checked before it is launched

`spec.check(&client)` applies the rules a launch needs, then the checks of the
forge, the kit policy of the daemon included (`kit.requireSignature`,
`kit.allowLocalKits`, `kit.allowedSources`). Nothing is pulled or written; the
mixins of a directory are read. A spec breaking a rule is an
`Error::InvalidSpec` that names the field:

| Rule | Why |
|---|---|
| exactly one of `agent`, `base` and `package` | there is no kit otherwise |
| a package that suits the daemon, without `args`, and without mixins of its own when the spec adds some | a package keeps the arguments it was compiled with, and the forge takes its base kit alone |
| MCP servers named with `agent`, claude, copilot or opencode | they are registered in an agent that has an adapter |
| every host of `network` a host pattern | the daemon stores `https://host` and `host/path` and never applies them |
| no refusal covering the host of a key or of an MCP server | a refusal wins: the connection would never go through |
| the variables of `keys`, `env` and the tokens of host servers set once | one value reaches the sandbox |
| the token of a remote server set by a key or by `env` | the variable would not exist in the sandbox otherwise |
| every workspace and mount an absolute path | a mount names a real path |
| every mixin one the forge takes | known fields, commands and hosts the daemon applies |

`spec.check_keys(&keys)` checks the values given at launch (`Keys`): one for
each key of the spec, and none for a variable no key reads, most likely a
misspelt one. The arguments of the base kit are checked against its
descriptor when the kit is forged, as the descriptor comes with its image.

```rust,no_run
use docker_sandbox_blueprint::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let spec = SandboxSpec::agent(Agent::Claude)
        .with_arg("model", "claude-haiku-5-5")
        .with_workspace("/srv/project")
        .with_network_deny(["*.pastebin.com"])
        .with_key(KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic"))
        .with_mcp(McpServer::host("tools", 47813));
    let client = Client::connect().await?;
    spec.check(&client).await?;
    let keys = Keys::new().with_key("ANTHROPIC_API_KEY", std::env::var("ANTHROPIC_API_KEY")?);
    spec.check_keys(&keys)?;
    Ok(())
}
```

## Launched, and owned by a lease

`spec.launch(&client, name, &keys)` applies a spec in seven steps:

1. check the spec and the values of its keys;
2. forge its kit in memory, the base kit rebuilt from its image, its mixins,
   the mixin of its network rules and the one of its MCP servers;
3. draw a token for each MCP server of the host, 32 bytes of `/dev/urandom`
   in hexadecimal, in the variable the agent reads it from;
4. prepare a `ProxiedKey` for each key, its placeholder in its variable, its
   hosts those the kits declare for its service; a refusal of the spec that
   covers one of them is refused;
5. create the sandbox with the variables, tokens, placeholders, workspace,
   mounts and resources;
6. start it, which runs its startup commands, and push its keys to the proxy;
7. return the `Lease`, which holds its session.

Nothing is written on the host at any step. A failure once the sandbox
exists (steps 6 and 7) removes it, unless the spec says `keep_on_failure`, and
the `Error::Launch` names the sandbox, its id, the step and whether it stays.
A launch given up on the way, its future dropped by a timeout, a `select!` or
Ctrl-C, removes the sandbox it created all the same, by its id, kept by the
spec or not: once the sandbox exists, the rest of the launch runs in a task of
its own, as the creation does before it.

Forging a kit costs about a second, its base image exported again.
`spec.launch_with(&client, name, &keys, &cache)` takes it from a `KitCache`
instead: the sandboxes of a job, launched from one spec, forge their kit once.
A kit the forge does not cover, a local or Git base kit for instance, is
compiled once by the `sbx` CLI (`KitCompiler` of `docker-sandbox-kit`) and
launched from its package, `SandboxSpec::package(dir)`: as it is, or with the
mixins, network rules and MCP servers of the spec composed on its base kit.

```rust,no_run
use docker_sandbox_blueprint::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::connect().await?;
    let lease = SandboxSpec::agent(Agent::Shell)
        .with_workspace("/srv/project")
        .launch(&client, "job-42", &Keys::new())
        .await?;
    let output = lease.sandbox().exec(&ExecSpec::shell("ls")).await?;
    println!("{}", output.stdout);
    lease.shutdown().await?;
    Ok(())
}
```

| `Lease` | Gives |
|---|---|
| `sandbox()` | the `Sandbox`: commands, files, ports, the whole API of `docker-sandbox` |
| `created()` | the answer of the creation, `withheld` included |
| `agent()` | the agent its kit declares, whose `command(args)` runs a task |
| `report()` | what the launch did |
| `mcp_token(name)` | the token of a server of the host, which the program checks |
| `shutdown()` | closes the session and removes the sandbox, its rules and keys with it |
| `detach()` | the sandbox and its session, kept beyond the lease |

A lease dropped without `shutdown()` closes its session: the sandbox stops
about 30 s later and stays, named in a warning of `tracing`. No removal runs
in a hidden task.

The `Report` is serializable, for a program in another language, and never
holds the value of a key or of a token: the kits of the sandbox, the hosts its
rules allow and refuse (with a warning when the global policy of the daemon
allows every host, so that the allowances restrict nothing), where each key
goes and which optional keys were left out and why, the name, kind and agent
of each MCP server, and the services whose
credential the daemon withholds.

## From another language

A spec also has a JSON form (serde), for a program in another language that
hands its sandboxes to a Rust one; a Rust program never needs it. The fields
are those of the methods, and an unknown one is refused:

```json
{
  "agent": "claude",
  "args": { "model": "claude-haiku-5-5" },
  "mixins": [{ "dir": "/srv/kits/project-tools" }],
  "workspace": { "mount": "/srv/project" },
  "mounts": [{ "dir": "/srv/docs", "read_only": true, "at": "/docs" }],
  "network": { "deny": ["*.pastebin.com"] },
  "keys": [{ "env": "ANTHROPIC_API_KEY", "service": "anthropic" }],
  "mcp": [{ "kind": "host", "name": "tools", "port": 47813 }],
  "env": { "LOG": "debug" },
  "resources": { "cpus": 2, "memory": "4g" },
  "keep_on_failure": false
}
```

A mixin is `{"dir"}` or `{"reference", "descriptor"}`, the workspace
`{"mount"}` or `{"clone"}`, a key `{"env", "service"}` or `{"env", "hosts"}`,
an MCP server its JSON form by kind. A compiled package as base kit is
`{"package": dir}`, in place of `agent` or `base`.

## Scope

The crate is the declarative layer: a description, its checks, its
application through the socket and the handle that owns the result, one
sandbox at a time, with nothing kept outside the daemon. Runners, queues, scheduling, retries, pools of sandboxes
and any HTTP API or command line on top live in another repository.

## Version and errors

The crate shares the version of `docker-sandbox` and `docker-sandbox-kit`: use
the three at the same version. Its `Error` keeps the errors of the two other
crates as they are, in `Error::Client` and `Error::Kit`.

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
