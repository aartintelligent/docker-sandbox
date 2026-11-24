# docker-sandbox

[![Crates.io](https://img.shields.io/crates/v/docker-sandbox.svg)](https://crates.io/crates/docker-sandbox)
[![docs.rs](https://img.shields.io/docsrs/docker-sandbox)](https://docs.rs/docker-sandbox)
[![CI](https://github.com/aartintelligent/docker-sandbox/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/aartintelligent/docker-sandbox/actions/workflows/ci.yml?query=branch%3Amain)
[![codecov](https://codecov.io/gh/aartintelligent/docker-sandbox/branch/main/graph/badge.svg)](https://codecov.io/gh/aartintelligent/docker-sandbox)
[![Security audit](https://github.com/aartintelligent/docker-sandbox/actions/workflows/audit.yml/badge.svg?branch=main)](https://github.com/aartintelligent/docker-sandbox/actions/workflows/audit.yml?query=branch%3Amain)
[![MSRV](https://img.shields.io/crates/msrv/docker-sandbox)](https://crates.io/crates/docker-sandbox)
[![License](https://img.shields.io/crates/l/docker-sandbox.svg)](#license)

Rust tooling to drive [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/)
(`sbx`) through the private `sandboxd` Unix socket, without the `sbx` CLI.

```text
docker-sandbox/   the library: a typed async client of the socket
examples/         complete programs, one crate each, in a workspace of their own
audit/            the audit of the daemon API, a tool that is never published
```

- **[`docker-sandbox`](docker-sandbox/README.md)** creates sandboxes from
  compiled kits, keeps them running, runs commands and agent sessions, moves
  files in and out, publishes ports, pushes credentials and scopes network
  access. Its README shows the API at a glance and what a runner needs to know.
- **[`examples`](examples/README.md)** are runnable programs: the basics, shell
  sandboxes (commands, files, network policy, ports, interactive shell,
  events), the MCP gateway checked from inside a sandbox with an `rmcp` client
  (local, remote with a credential, static and dynamic servers), and every
  coding agent `sbx` knows — Claude Code, Codex, OpenCode, Copilot, Cursor,
  Devin, Docker Agent, Droid, Kiro — interactively, with and without an MCP
  server, or unattended on a task whose tests check the agent's work. Run one
  with `just example <name>`.
- **[`audit`](audit/README.md)** replays scenario suites against the live
  daemon, records one snapshot per `sbx` version with the measured contract
  (`audit/snapshots/<version>/api/`), compares versions and exports the test
  fixtures of the library. Its README is the workflow to follow when `sbx`
  changes.

> This project is not affiliated with Docker, Inc. The `sandboxd` socket API is
> private and changes with every `sbx` release: the library is validated
> against the version recorded in `audit/snapshots/`.

## Requirements

- Rust 1.88 or later (`rust-toolchain.toml` pins the development toolchain).
- Linux or macOS: the daemon is reached through a Unix socket.
- Docker Sandboxes installed, for the examples, the live tests and the audit.
  Run them as the user owning the daemon, never as root.

## Development

[`CONTRIBUTING.md`](CONTRIBUTING.md) lists the tooling, the quality gates and
the commit conventions; `just` alone lists the recipes, and `just ci` runs
every local gate.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
