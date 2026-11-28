# AGENTS.md

Guidance for AI coding agents working in this repository. `CLAUDE.md` is a symlink to this file, so
every tool reads the same instructions.

## What this is

The docker-sandbox repository: the `docker-sandbox` library crate, which drives Docker Sandboxes
(`sbx`) through the private `sandboxd` Unix socket; `docker-sandbox-kit`, which writes the kit
packages it replays (what the `sbx` CLI does on the client side, as the daemon cannot resolve kits
by itself); `docker-sandbox-blueprint`, which describes a whole sandbox as data; their examples;
and `audit`, the tool that maps that socket. `docker-sandbox` holds what the daemon API offers,
and `docker-sandbox-kit` the client-side work of the CLI without which the daemon cannot be used;
the secret store and the other files of the CLI stay out of both. `docker-sandbox-blueprint`
describes a whole sandbox as data and applies it through the two other crates, owning the result in
a `Lease`: one sandbox at a time, nothing kept outside the daemon. Runners, queues, scheduling and retries stay in
another repository.
**Nothing else belongs here**: runners, operators, orchestration, agent management and any HTTP API
built on top of the library live in another repository (project rule).

The repository is laid out as axum is: a virtual workspace at the root, one directory per crate,
and the examples in a workspace of their own:

- `docker-sandbox/` (`src/`, `tests/`, its `README.md` and `CHANGELOG.md`, which release-plz
  writes from the Conventional Commits, never by hand), one module per domain
  of the daemon API:
  - `client/`: `Client`, `ClientBuilder`, socket resolution, API versions;
  - `sandbox/`: the `Sandbox` handle and `Sandboxes` collection, creation, life cycle, ports,
    credentials, sessions, files, SSH, kit changes (in place or by container swap);
  - `exec/`: specs, outputs, interactive sessions and the private stdcopy decoder;
  - `policy/`, `image/`, `event/`, `daemon/`, `mcp.rs`, `prompt.rs`;
  - `kit/`: kit packages, as the daemon replays them, and the agent their base kit declares;
  - `transport/` (private): HTTP/1.1 over the socket, upgrades, NDJSON streams;
  - `error.rs`, `prelude.rs`.

  `tests/` has one file per domain against the fake daemon of `tests/support/`, plus `fixtures.rs`
  (daemon responses exported from the audit) and `live.rs` (feature `live`), with
  `tests/live-probe/`, the program the live tests upload and run inside sandboxes (a workspace
  member, built statically by `just test-live`).
- `docker-sandbox-kit/` (the version of `docker-sandbox`, which release-plz gives both crates;
  `README.md`, `CHANGELOG.md`):
  - `forge/` (feature `forge`, default): `KitForge`, packages written without the CLI, the base
    kit rebuilt from its published image and mixins from kit descriptors (`KitDescriptor`,
    `LifecycleStep`, `Mixin`), MCP servers registered in the agent by a mixin (`McpServer`,
    `Mixin::mcp`, claude, copilot and opencode), packages kept by composition (`KitCache`);
  - `compiler/` (feature `compiler`): `KitCompiler`, packages compiled once by the CLI behind a
    capturing proxy;
  - `reference.rs`: references read from their name as the CLI reads them, `KitRef` (a kit, as
    `sbx create <kit>` and `--kit <kit>` read it);
  - `error.rs`: its `Error`, whose `Client` variant keeps the errors of `docker-sandbox`;
  - `prelude.rs`: its types, each behind its feature, without its `Error` and `Result`, to be
    glob-imported beside the prelude of `docker-sandbox`.

  `tests/` holds `forge.rs` and `compiler.rs` (the test binary re-executed as a fake `sbx`) against
  the fake daemon of `docker-sandbox/tests/support/`, included by path rather than copied, and
  `live.rs` (feature `live`), its packages forged without the CLI.
- `docker-sandbox-blueprint/` (the version of the two others; `README.md`, `CHANGELOG.md`), one
  module per domain of a spec, each owning its fields, its `impl SandboxSpec` blocks and its rules:
  - `spec/`: `SandboxSpec`, its JSON form and the fields of the sandbox itself (kit, workspace,
    mounts, variables, resources), the mixins of the program (`mixin.rs`), and `check.rs`, the
    rules that span the domains and the kit a spec forges;
  - `network/`: the allowed and refused hosts, their generated `network-policy@1` mixin, the
    refusals that would cut a key or a server off and `NetworkReport`, `host.rs` (private)
    matching the patterns, `http.rs` the HTTP rules a launch applies once the sandbox runs;
  - `key.rs`: `KeySpec`, where an API key goes, `Keys`, the values given at launch, the
    `ProxiedKey`s of a launch and `KeyReport`;
  - `mcp/`: the MCP servers of the agent, their mixin, the variables of their tokens and
    `ServerReport`, `token.rs` drawing the token of a server of the host at each launch;
  - `launch/`: `SandboxSpec::launch` and `launch_with` (its kit from a `KitCache`), its seven
    steps and the removal of a failed launch,
    `lease.rs` (`Lease`, which owns the sandbox and its session) and `report.rs` (`Report`, which
    puts the parts of the domains together, never a secret value);
  - `error.rs`: its `Error`, which keeps the errors of the two other crates, and `LaunchStep`.
  - `prelude.rs`: what a spec is written with and the lease that runs it, the types of the two
    other crates included.

  `tests/spec.rs` and `tests/launch.rs` run against the fake daemon of
  `docker-sandbox/tests/support/` and the kit images of `docker-sandbox-kit/tests/images/`, and
  `live.rs` (feature `live`) shares the plumbing of `docker-sandbox-kit/tests/live_support/`, all
  included by path.
- `audit/`: the `audit` binary, never published. Suites, kits and
  inputs are embedded from `assets/`; each audited `sbx` version gets a snapshot in
  `snapshots/<version>/`. Its README is the upgrade workflow.
- `examples/`: a separate workspace (`examples/Cargo.toml`, its own `Cargo.lock`), one binary crate
  `example-<directory>` per example: `base`, `daemon`, `images`, `shell-*`, `mcp-*`,
  `agent-<agent>`, `agent-<agent>-mcp` and `agent-<agent>-automation` for every agent `sbx` knows,
  `agent-kit-args`, `agent-kit-forge` and `shell-forge`, and `blueprint`, an agentic task launched
  from a `SandboxSpec`. The agent examples, `agent-kit-forge`, `shell-forge` and `blueprint` forge
  their packages without the CLI; the others compile theirs once with it. Together they call the
  public functions of the published crates. Programs run
  inside a sandbox are static musl binaries of the workspace, built and installed by
  `common/src/program.rs`: `mcp-client` (the gateway checked from inside by the `mcp-*` examples)
  and `sandbox-tools` (the web server and HTTP client of `shell-ports` and `shell-secrets`).
  `shell-clone` and `shell-ssh` run their own binary again, as the `uploadpack` of `git fetch` and
  the `ProxyCommand` of `ssh`, to relay those through the socket. `common/` holds the shared
  plumbing (kit cache, mixin and argument compositions included, throw-away sandboxes, programs
  built for a sandbox, terminal relay, credentials, the `rmcp` servers — standard input and
  output, streamable HTTP — and the in-sandbox client, the unattended flow and its `task/`).
  Examples create real sandboxes: they are linted in CI, run locally with `just example <name>`.
  The MCP examples store dummy `mcp:<server>:…` secrets and remove them, `:endpoint` binding
  included; `shell-secrets` pushes dummy secrets through the socket only, and they go with its
  sandbox. Examples that change global state (`shell-policy`, `daemon`, `images`) restore it.

`CONTRIBUTING.md` is the source of truth for the workflow (gates, hooks, commits, pull requests);
this file covers architecture and conventions.

## Working mode and attribution

- Routine changes that stay within the written conventions proceed on their own. A design decision
  (new dependency, public API change, new module layout, pattern or convention) is proposed to the
  maintainer and validated before implementation.
- Commit messages, PR titles, descriptions and review comments never mention the AI tooling: no
  "Claude", no `Co-Authored-By` trailer, no "Generated with" footer. This rule overrides any default
  attribution guidance; authorship is only the configured git user.
- Never weaken a workspace lint, `clippy.toml`, `deny.toml` or a test to make something pass.

## Commands

```bash
just                 # list the recipes
just ci              # every local gate: fmt-check clippy typos test doctest coverage deny privacy
just test            # nextest, no daemon needed (fakes and fixtures)
just coverage        # every line of the two published crates run by their tests
just test-live       # the live tests of both crates, against the real daemon of the current user
just state-save      # record the state of the daemon before a run that changes it
just state-check     # check it after
just audit           # replay the suites into audit/snapshots/<sbx version>/
just scenarios agents          # replay one suite alone; then `just requests agents`
just upgrade v0.54.0 v0.55.0   # audit, compare and fixtures for a new sbx version
```

The procedures that work on the daemon (`audit/README.md`) have Claude Code skills in
`.claude/skills/`, invoked by hand only: `/sbx-live` (any run against the daemon, its state
recorded and checked), `/sbx-measure` (measure a CLI feature, then code it) and `/sbx-upgrade`
(a new `sbx` version). They follow the README and add no rule of their own.

Never run the test suite with `--all-features`: it turns on the `live` feature, whose tests create
sandboxes on the real daemon. Recompile `audit` before an audit (`cargo run` does):
the suites are embedded in the binary, and two suite steps run that same binary.

## The daemon

- Measured on `sbx` v0.54.1, API 0.52.0, as v0.54.0 was before it (`audit/snapshots/` keeps
  both). Anything about the daemon comes from an audit snapshot, not from memory: a new route or
  field goes into a suite first, then into the library, with a fixture.
- A sandbox only stays running while a `GET /sandbox/{name}/session` stream is open, and stops about
  30 s after the last one closes. `POST /sandbox/{name}/start` is idempotent.
- `POST /sandbox` answers HTTP 200 and reports failures inside its NDJSON stream.
- Never call `/daemon/reset`, `/daemon/shutdown` or `POST /policy/network/setup`. The library does
  not expose them and the audit refuses the first two (`NEVER_PROBE`).
- Run `sbx`, the kit compiler and the audit as the user owning the daemon, never as root: `sbx`
  ignores `DOCKER_SANDBOXES_API` as root, which the kit compiler and the spy rely on.
- Only dummy secret values in suites and tests; never read or change the user's stored secrets.

## Personal data

Nothing committed may hold personal data: home directory, user or host name, e-mail address, host
session variables (`SBX_HOST_*`), D-Bus GUIDs. The one exception is the public contact of the
maintainer, in `authors` of the root `Cargo.toml` (published with the crate) and in
`CODE_OF_CONDUCT.md`. The spy scrubs captures when it records them, `audit privacy-check`
(`just privacy`) checks the snapshots, the fixtures and the files `cargo package` publishes for
`docker-sandbox` (the declared contact aside), and the pre-commit hook runs it. Scenario paths
live under `$TMPDIR/docker-sandbox-audit-work`, never under the home directory.

## Comment conventions (same as the user's other projects)

- English only, full sentences ending with a period, code in backticks, em dash for asides.
- TOML, justfile, YAML, dotfiles: open with an 80-column banner block (`# ` + 78 hyphens, a
  `# <Name> — <role>.` line, optional paragraphs separated by a bare `#`, closing banner, blank
  line). Comments sit on their own line above the key they describe, never inline. `=` aligned
  (`taplo fmt`).
- Root `Cargo.toml`: 3-line banner blocks separate "Workspace dependencies", "Quality gates" and
  "Build profiles". Every `[workspace.dependencies]` entry carries a paragraph saying what it is
  for, which crate needs it and why its feature set is what it is. Member manifests comment only
  non-obvious dependencies and put `# Quality gates are centralized in the workspace root
  \`Cargo.toml\`.` above `[lints]`.
- Rust: no banners inside files. Every file opens with a `//!` header (one-sentence summary, blank
  `//!`, then why / ownership paragraphs). File order: header → `mod`/`pub use` → `use` groups
  (std, external, crate) → documented private consts → types → impls → `#[cfg(test)]` last.
- `///` on every item including private ones, fields and variants; third-person summaries
  ("Builds …") or noun phrases; `# Errors` says when each error is returned; `# Panics` starts
  with "When …". Error messages are lowercase without trailing period.
- Inline `//` comments explain why or the hazard, on their own line above the block. `#[allow]`
  carries `reason = "…"`. No TODO/FIXME markers.

## Conventions and gotchas

- Workspace lints: `unsafe_code = forbid`, `missing_docs`, `unreachable_pub`, clippy `pedantic`,
  `unwrap_used` / `todo` / `print_*` warn; CI runs with `-D warnings`. Tests may `unwrap`
  (clippy.toml); integration-test files allow it at their root, with a reason, for their helpers.
  The `audit` binary allows printing at its crate root.
- Domain modules: each module owns its payload types and the methods of the handles that use
  them, in separate `impl Sandbox` blocks (exec methods live in `exec/`, network rules in
  `policy/`, …). Handles (`Sandbox`, `Daemon`, `Images`, …) own a `Client` clone and stay
  `Clone + Send + Sync + 'static`; `tests/client.rs` checks it. Every request names its operation
  (`Request::post("sandbox start", …)`), which errors and logs carry.
- Builders: request and option types are plain data built with consuming `with_*` methods marked
  `#[must_use]`; removals are `without_*`. Text parameters take `impl Into<String>`, lists
  `impl IntoIterator<Item = impl Into<String>>`, paths `impl Into<PathBuf>`. Enum variants that
  carry data have named constructors.
- Durations are `std::time::Duration`; fields stored in seconds on the wire are rounded up.
- Locks are poison-tolerant (`lock().unwrap_or_else(PoisonError::into_inner)`).
- Unknown daemon fields are kept (`extra` maps of `serde_json::Value`) so that a new daemon version
  does not break decoding; `#[non_exhaustive]` on public enums and errors.
- The library checks `api_version` when it connects (`SUPPORTED_API`); widen the range only after
  an audit of the new version.
