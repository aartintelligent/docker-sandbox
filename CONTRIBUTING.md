# Contributing

Thanks for contributing to docker-sbx: `docker-sandbox`, a Rust client for the
`sandboxd` socket of Docker Sandboxes (`sbx`), and `audit`, the tool that
maps that socket. This guide covers how to set up the repository, the quality
gates, the commit conventions and how a change is validated before it is
merged. It applies to both human contributors and AI coding agents.

> Code conventions and their rationale live in [`AGENTS.md`](AGENTS.md); the
> upgrade workflow in [`audit/README.md`](audit/README.md).
> Read them before writing code. This file is the source of truth for the
> workflow.

## Scope

This repository holds the socket library and its audit tool only. Runners,
operators, orchestration and HTTP APIs built on the library belong to other
repositories.

## Prerequisites

- **Rust**, managed with [rustup](https://rustup.rs). `rust-toolchain.toml`
  pins the development toolchain (with `clippy` and `rustfmt`); rustup
  provisions it on the first `cargo` invocation. The crates must also build on
  the MSRV declared as `rust-version` in `Cargo.toml`, which CI checks
  separately.
- **[just](https://just.systems)**: the `justfile` wraps every command below
  into short recipes; `just` alone lists them.
- **[lefthook](https://lefthook.dev)** (git hooks) and
  **[committed](https://github.com/crate-ci/committed)** (Conventional Commits
  linter).
- For the full local CI run: `cargo-nextest`, `cargo-llvm-cov`, `cargo-deny`,
  `cargo-hack`, `taplo-cli` and `typos-cli` (`cargo install cargo-nextest
  cargo-llvm-cov cargo-deny cargo-hack taplo-cli typos-cli committed`), the
  LLVM tools for the coverage (`rustup component add llvm-tools`), and a
  nightly toolchain for the docs.rs build (`rustup toolchain install nightly`).
- For the live tests and the audit only: Docker Sandboxes (`sbx`) installed and
  logged in, on Linux or macOS, as a user who may create and remove test
  sandboxes.
- For the live tests and the MCP examples: the musl target of the sandbox
  platform (`rustup target add x86_64-unknown-linux-musl`, or `aarch64-…`),
  to build the programs they run inside sandboxes (`live-probe`,
  `mcp-client`).

## Getting started

```sh
just setup                # installs the git hooks
cargo build --workspace   # the first build provisions the pinned toolchain
just test
```

The default test suite needs no daemon: it runs against an in-memory fake
daemon and recorded fixtures.

## Quality gates

Every gate below must stay green. `just ci` chains the local ones; CI runs them
as described in [Pipeline](#pipeline), and `CI OK` is the single required
status check.

| Gate | Recipe | What it guards |
|---|---|---|
| Format | `just fmt-check` | `cargo fmt` and `taplo fmt`, policies in `rustfmt.toml` and `.taplo.toml` |
| Lint | `just clippy` | Workspace lints and `clippy::pedantic`, warnings are errors |
| Spelling | `just typos` | Sources and docs (`typos.toml`) |
| Tests | `just test`, `just doctest` | The suite on Linux and macOS, plus the doctests |
| Coverage | `just coverage` | Every line and 95 % of the regions of the library run under its tests ([Testing](#testing)); the report goes to Codecov |
| Examples | `just examples-clippy` | The examples workspace builds and passes clippy |
| Docs | `just doc` | Nightly rustdoc with the docs.rs flags, warnings and broken links are errors |
| Supply chain | `just deny` | Advisories, licences, bans and sources (`deny.toml`); licences and sources of the examples |
| Privacy | `just privacy` | No personal data in the audit snapshots, the fixtures and the files of the published crate (the contact in `authors` aside) |
| Features | `just hack` | Every feature combination of the library compiles |
| MSRV | `just msrv` | The workspace compiles on `rust-version` |
| Commits | CI only | `committed` validates every commit of a pull request, except Dependabot's |

The live tests (`just test-live`) are not part of CI: they need a real daemon.
Run them before merging a change to the library; the recipe records the state
of the daemon before them and checks it after.

## Pipeline

Each stage checks what the previous one could not, and nothing is rebuilt
without a reason:

| Stage | Where | What runs |
|---|---|---|
| Commit | lefthook | `pre-commit`: formatting, clippy, and the privacy check when audit data or the published crate changes; `commit-msg`: Conventional Commits |
| Pull request | `ci.yml` | Only the jobs the changed files concern (below); format, spelling and commits always |
| Merge queue, `main` | `ci.yml` | Every job, so that `main` is always fully checked and the caches pull requests restore stay warm |
| Release | `release-plz.yml` | On `main`: the release PR (version bump, changelog), checked by CI like any other; once merged, the tag and the GitHub release |
| Weekly | `audit.yml`, Dependabot | RustSec advisories of both lockfiles; dependency and Actions update PRs |

On a pull request, the `changes` job maps the changed files to areas:

| Area | Files | Jobs |
|---|---|---|
| Library | `docker-sandbox/`: manifest, README (doctests), `src/`, `tests/` | Tests on macOS too, coverage, docs, feature powerset |
| Rust | the library, and `audit/`: manifest, `src/`, `assets/` | Clippy, tests on Linux, MSRV |
| Examples | `examples/**`, the library manifest and `src/` | Examples |
| Audit data | `audit/`: manifest, `src/`, `snapshots/`; the fixtures | Privacy |
| Dependencies | any `Cargo.toml` or `Cargo.lock`, `deny.toml` | cargo-deny |

The root manifest and lockfile, `rust-toolchain.toml`, `clippy.toml` and
`ci.yml` count as a change of every Rust area. A documentation-only change thus
runs format, spelling and commits, without compiling anything. Every job
restores the cache that `main` saved, and rebuilds only the crates that
changed.

The examples' advisories do not block CI: a transitive dependency of `rmcp`
can be unmaintained with no upgrade available. The weekly audit reports them,
and `just deny-examples` checks them locally.

## Repository layout

```text
Cargo.toml                the workspace: members, shared metadata, dependency versions, lints
docker-sandbox/           the library: src/ (one module per domain), tests/ (fixtures/ holds daemon responses)
docker-sandbox/tests/live-probe/  the program the live tests run inside sandboxes
audit/                    the audit tool
audit/assets/             scenario suites, test kit and inputs, embedded in the binary
audit/snapshots/<v>/      one audit per sbx version: captures, router inventory, contract
examples/                 a workspace of its own: one crate per example, common/ for shared helpers
.claude/skills/          Claude Code skills of the procedures of audit/README.md
```

The layout follows axum: the root `Cargo.toml` is a virtual workspace and each
crate has its own directory. Both members share the version, edition, MSRV,
licence and lints declared there (`[workspace.package]`, `[workspace.lints]`).
Every external dependency version lives once in `[workspace.dependencies]`,
with a comment stating which crate needs it and why its feature set is what it
is; a member never declares a version of its own. The examples have their own
workspace manifest, built the same way, so that their dependencies stay out of
the library's lockfile, dependency policy and MSRV; `just examples-clippy`
lints them and `just example <name>` runs one against the real daemon.

## Development rules

- **The daemon API is measured, not guessed.** A new daemon route or field goes
  into an audit suite first (`audit/assets/suites/`), then into the
  library, with a fixture. `tests/fixtures.rs` fails until every exported
  fixture is decoded or listed as not exposed.
- **One module per domain.** A route belongs to the module of its payloads,
  which also holds the handle methods that call it; see the crate
  documentation for the map.
- **When `sbx` changes**: `just upgrade <old> <new>` (audit, API changes,
  fixtures), then adapt the library until the tests pass, and run the live
  tests. A feature of the CLI is measured with a suite before it is coded:
  [`audit/README.md`](audit/README.md) has both procedures.
- **The daemon is the user's own**: record its state before a run that changes
  it and check it after (`just state-save`, `just state-check`; `just test-live`
  does it by itself). Claude Code users have the same procedures as skills
  (`/sbx-live`, `/sbx-measure`, `/sbx-upgrade`).
- **No personal data** in anything committed: captures are scrubbed when
  recorded and `just privacy` must stay green.
- **Never call** `/daemon/reset`, `/daemon/shutdown` or the global network
  policy setup from code or tests.
- **Rust only** in the repository: no Python or shell scripts; tools are Rust
  subcommands of `audit` or `just` recipes.
- Public types are plain data with fluent `with_*` builders; text parameters
  take `impl Into<String>`.

## Comments and documentation

English only. Every item, public or private, carries rustdoc; every file opens
with a `//!` header. TOML, YAML and `justfile` files open with a banner block
and put each comment on its own line above the key it describes. The details
are in [`AGENTS.md`](AGENTS.md).

## Testing

- Unit tests sit next to the code (`#[cfg(test)]` at the end of the file).
- Integration tests are in `tests/`, one file per domain against an in-memory
  fake daemon on a Unix socket (`tests/support/`), and the decoding of every
  recorded daemon response (`fixtures.rs`); refresh the fixtures with
  `just fixtures <version>`. The fake daemon serves canned replies, in turn
  when a route gets several, and plays slow, truncated or hung-up connections.
- What a test cannot do in its own process (change the environment, run the
  `sbx` CLI) it does in a helper process: the test binary re-executes itself
  to run one test, as Go's `os/exec` tests do. `compiler.rs` plays the CLI
  that way.
- Live tests (feature `live`, `tests/live.rs`) create and remove their own
  sandboxes and images.

### Coverage

Every line of `docker-sandbox` runs under the tests above, without a daemon:
`just coverage` fails on the first line none of them runs, and lists it. CI
runs the same gate when the library or `codecov.yml` changes, and uploads the report to
[Codecov](https://codecov.io) (`codecov.yml`), which annotates the uncovered
lines of a pull request. The doctests, the live tests and the examples do not count; the test
files themselves are not measured.

The gate also holds the regions at 95 % or more: the branches within a line,
such as the error of a `?` or of a `map_err`, which line coverage counts as
run as soon as the line is. Some of them cannot be provoked by a test (an I/O
failure of the host); the floor keeps the others from eroding.

New code comes with the tests that run it. A line no test can reach is a
design question, settled in the code (a branch that cannot happen goes away),
never by excluding it from the measure.

The line percentage in the table of `cargo llvm-cov` is not the gate. The
library is built twice, once for its unit tests and once for the integration
tests, and for each function the table keeps the better-covered build rather
than their union: it can stay under 100 % while every line runs. The gate
counts the lines no test runs, as Codecov does. The regions floor applies to
the percentage of the table, which errs on the low side in the same way.

Codecov authenticates the upload through OpenID Connect: the repository needs
the Codecov GitHub app, and no token secret. Its statuses must not be required
by branch protection, since a pull request that leaves the library untouched
uploads nothing; `CI OK` already includes the gate.

## Git hooks

`just setup` installs two hooks (`lefthook.yml`): `pre-commit` runs the
formatting checks, clippy and the privacy check; `commit-msg` runs `committed`.

## Commit conventions

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):
`<type>(<scope>): <description>`, lowercase, at most 100 columns, the body
wrapped at 100 columns. Scopes are `sandbox` (the library), `audit` or
`workspace`.

| Type | Use for | Changelog |
|---|---|---|
| `feat` | A new capability | Added |
| `fix` | A bug fix | Fixed |
| `perf` | A performance improvement | Performance |
| `refactor` | A change that neither fixes nor adds | Changed |
| `docs` | Documentation | Documentation |
| `deprecate` | A deprecation | Deprecated |
| `revert` | A revert | Reverted |
| `test`, `build`, `ci`, `chore`, `style` | Everything else | skipped |

A breaking change is marked with `!` (`feat(sandbox)!: …`) and explained in a
`BREAKING CHANGE:` footer.

The changelog (`docker-sandbox/CHANGELOG.md`) is never edited by hand:
release-plz writes a section per release from these messages, the 0.x ones
included, so the message of a commit is its changelog entry.

## Pull requests

One topic per pull request, the title being a Conventional Commit (it becomes
the squash commit). Fill in the template; `CI OK` must be green.

## Releasing

[release-plz](https://release-plz.dev) opens a release pull request from the
Conventional Commits on `main`; merging it publishes `docker-sandbox` to
crates.io, tags the version (`v<version>`) and creates the GitHub release. The
other members are never published (`publish = false`).

crates.io trusts the release workflow (trusted publishing: `release-plz.yml`,
environment `release`), so no crates.io token is stored. A new crate cannot be
published that way: 0.1.0, tagged before the crate was publishable, is
published by hand from `main` with `cargo publish --locked -p docker-sandbox`.

## License

By contributing, you agree that your contributions are dual licensed under
MIT OR Apache-2.0, as the rest of the project.
