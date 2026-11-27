# ------------------------------------------------------------------------------
# just — development recipes mirroring what CI runs.
#
# Install `just` with `cargo install just`; `just` alone lists the recipes.
# Recipes shell out through `bash -euo pipefail`, so any failing step aborts.
# ------------------------------------------------------------------------------

set shell := ["bash", "-euo", "pipefail", "-c"]

# Target of the programs run inside sandboxes: Linux, static, on the architecture of the host.
live_target := `uname -m | sed 's/arm64/aarch64/'` + "-unknown-linux-musl"

# Every feature of the two published crates but `live`, which needs a real daemon.
features := "docker-sandbox/secrecy,docker-sandbox-kit/compiler,docker-sandbox-kit/forge"

# List the available recipes.
default:
    @just --list

# Install the git hooks (lefthook.yml); run once after cloning.
setup:
    lefthook install

# ------------------------------------------------------------------------------
# Formatting & linting — the cheap checks to run before every commit.
# ------------------------------------------------------------------------------

# Format Rust and TOML sources in place, examples included.
fmt:
    cargo fmt --all
    cargo fmt --all --manifest-path examples/Cargo.toml
    taplo fmt

# Verify formatting without writing anything.
fmt-check:
    cargo fmt --all --check
    cargo fmt --all --check --manifest-path examples/Cargo.toml
    taplo fmt --check

# Lint with clippy, treating warnings as errors.
clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Check the spelling of sources and documentation.
typos:
    typos

# ------------------------------------------------------------------------------
# Tests — unit, integration and documentation tests, plus the docs build.
# ------------------------------------------------------------------------------

# Run the test suite with nextest, falling back to cargo test. The `live`
# feature stays off: it needs a real daemon (see `test-live`).
test:
    cargo nextest run --workspace --features {{features}} || cargo test --workspace --features {{features}}

# Run the doctests — nextest does not run them.
doctest:
    cargo test --workspace --features {{features}} --doc

# Check that the tests run every line of the two published crates and at least 95 % of their
# regions, as CI does, and list the lines they miss.
# Needs `cargo-llvm-cov` and `rustup component add llvm-tools`; `just coverage-html` browses the
# report. The line percentage of the table can stay under 100 % with every line run: for each
# function, `llvm-cov` keeps its best-covered build (unit or integration tests), not their union.
coverage:
    cargo llvm-cov nextest -p docker-sandbox -p docker-sandbox-kit --features {{features}} --show-missing-lines --fail-uncovered-lines 0 --fail-under-regions 95

# Write the coverage report of the two published crates as HTML and open it.
coverage-html:
    cargo llvm-cov nextest -p docker-sandbox -p docker-sandbox-kit --features {{features}} --html --open

# Run the integration tests against the real daemon and sbx CLI of the current user. They create
# and remove their own sandboxes and images; the state of the daemon is recorded before and
# checked after, even when a test fails. The program they run inside sandboxes, live-probe, is
# built statically for the architecture of the host (`rustup target add <arch>-unknown-linux-musl`).
# The live tests of docker-sandbox-kit forge their packages, without the CLI.
test-live:
    cargo run --quiet -p audit -- state --save
    cargo run --quiet -p audit -- kit-compile audit/assets/kits/audit-mixin-v3
    cargo build --quiet --release -p live-probe --target {{live_target}}
    status=0; DOCKER_SANDBOX_LIVE_PACKAGE="$PWD/target/audit/kits/audit-mixin-v3" DOCKER_SANDBOX_LIVE_PROBE="$PWD/target/{{live_target}}/release/live-probe" cargo test -p docker-sandbox --features live --test live -- --test-threads=1 || status=$?; cargo test -p docker-sandbox-kit --features live --test live -- --test-threads=1 || status=$?; cargo run --quiet -p audit -- state --check; exit $status

# CI pins a dated nightly (see .github/workflows/ci.yml) because newer nightlies break `filetime`;
# locally, any nightly at or before that date works.
# Build the API docs exactly as docs.rs would.
doc:
    RUSTDOCFLAGS="--cfg docsrs -D warnings" cargo +nightly-2026-10-14 doc -p docker-sandbox -p docker-sandbox-kit --features {{features}} --no-deps

# ------------------------------------------------------------------------------
# Supply chain & compatibility — dependency policy, feature combinations, MSRV.
# ------------------------------------------------------------------------------

# The examples are checked for licences and sources; `deny-examples` checks their advisories.
# Check the supply chain with cargo-deny: advisories, licences, bans and sources.
deny:
    cargo deny check
    cargo deny --manifest-path examples/Cargo.toml check licenses sources

# Check the advisories of the examples, as the weekly security workflow does.
deny-examples:
    cargo deny --manifest-path examples/Cargo.toml check advisories

# Check that every feature combination of the two published crates compiles on its own.
hack:
    cargo hack check -p docker-sandbox -p docker-sandbox-kit --feature-powerset --no-dev-deps

# Verify the crates build on the MSRV declared in Cargo.toml.
msrv:
    cargo +$(awk -F'"' '/^rust-version/ {print $2; exit}' Cargo.toml) check --workspace --all-features

# ------------------------------------------------------------------------------
# Examples — one crate per example, in a workspace of their own (examples/).
# ------------------------------------------------------------------------------

# Lint every example, treating warnings as errors.
examples-clippy:
    cargo clippy --manifest-path examples/Cargo.toml --workspace --all-targets -- -D warnings

# Run one example against the real daemon, e.g. `just example shell-exec` or
# `just example agent-claude-mcp`. Extra arguments go to the example.
example NAME *ARGS:
    cargo run --quiet --manifest-path examples/Cargo.toml -p example-{{NAME}} -- {{ARGS}}

# ------------------------------------------------------------------------------
# Audit — keeping the daemon API map up to date (audit/README.md).
# ------------------------------------------------------------------------------

# Replay every suite against the live daemon into a new snapshot. Extra arguments go to
# `audit audit`, e.g. `just audit --force`.
audit *ARGS:
    cargo run --quiet -p audit -- audit {{ARGS}}

# Audit the installed sbx version NEW and bring the fixtures up to it: snapshot, API changes since
# OLD (target/audit/compare-OLD-NEW.md), fixtures; then adapt docker-sandbox and docker-sandbox-kit
# until `just ci` passes (audit/README.md, "When `sbx` changes").
upgrade OLD NEW:
    cargo run --quiet -p audit -- audit
    cargo run --quiet -p audit -- compare audit/snapshots/{{OLD}} audit/snapshots/{{NEW}} --out target/audit/compare-{{OLD}}-{{NEW}}.md
    cargo run --quiet -p audit -- fixtures audit/snapshots/{{NEW}}
    @echo "API changes: target/audit/compare-{{OLD}}-{{NEW}}.md; kit artifacts: audit/snapshots/{{NEW}}/results/kits.json; next: adapt docker-sandbox and docker-sandbox-kit until 'just ci' passes, then 'just test-live'"

# List the API changes between two snapshot versions, e.g. `just compare v0.54.0 v0.55.0`.
compare OLD NEW:
    cargo run --quiet -p audit -- compare audit/snapshots/{{OLD}} audit/snapshots/{{NEW}}

# Refresh the decoding fixtures of the library from a snapshot version.
fixtures VERSION:
    cargo run --quiet -p audit -- fixtures audit/snapshots/{{VERSION}}

# Replay one suite alone, out of the snapshot, e.g. `just scenarios agents` or
# `just scenarios agents --only create-r,run-r-flag-args`; the daemon is restored afterwards.
scenarios SUITE *ARGS:
    cargo run --quiet -p audit -- scenarios --suite {{SUITE}} {{ARGS}}

# List what a replayed suite sent to the daemon, scenario by scenario, e.g.
# `just requests agents --only create-w-workspaces`; `--reads` adds the GET requests.
requests SUITE *ARGS:
    cargo run --quiet -p audit -- requests target/audit/scenarios/{{SUITE}}.jsonl {{ARGS}}

# Record the state of the daemon: sandboxes, images, rules, settings, secrets and MCP servers.
state-save:
    cargo run --quiet -p audit -- state --save

# Check that the daemon is back to the recorded state, naming the runs of its own image collector.
state-check:
    cargo run --quiet -p audit -- state --check

# Look for personal data in the snapshots, the fixtures and the files of the published crate.
privacy:
    cargo run --quiet -p audit -- privacy-check

# ------------------------------------------------------------------------------
# Everything CI runs — the aggregate gate.
# ------------------------------------------------------------------------------

# Run every check CI runs, locally.
ci: fmt-check clippy examples-clippy typos test doctest coverage deny privacy
