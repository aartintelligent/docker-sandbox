---
name: sbx-measure
description: Measure what the sbx CLI sends to the sandboxd socket for a feature — a flag of `sbx run` or `sbx create`, a field, a route — with an audit suite, then code it from the measured bodies, in docker-sandbox or, for a kit artifact or descriptor, in docker-sandbox-kit. Use when the library must learn something the CLI does.
argument-hint: the CLI feature, e.g. "sbx create --mount"
disable-model-invocation: true
---

# Measuring a CLI feature before coding it

The procedure is `audit/README.md`, section "Measuring a CLI feature before
coding it"; read it first, and `AGENTS.md` for the conventions. Feature:
$ARGUMENTS

1. Read the Docker Sandboxes documentation and `sbx <command> --help` of the
   installed version; note what the feature should change on the daemon.
2. Write scenarios with the shell kit only, sandbox names starting with
   `audit-`, dummy values, inputs in `audit/assets/fixtures/` (embedded in
   `audit/src/assets.rs`).
3. Replay them as the `sbx-live` skill says (state recorded and checked):
   `just scenarios <suite>`, then `just requests <suite> --only <scenarios>`
   to read the bodies, and `target/audit/scenarios/<suite>.json` for what each
   scenario printed. Iterate with `--only`.
4. Write down in `audit/assets/notes.md` what the captures cannot show.
5. Record: `just audit --force` (background), `just fixtures <version>`,
   `just privacy`.
6. Propose the library change to the maintainer before coding it when it adds
   public API (`AGENTS.md`, "Working mode"). Then code it with a test asserting
   the measured body against the fake daemon, and keep `just coverage` green.
   When the feature lies in a kit artifact, it belongs to
   `docker-sandbox-kit`: check that it forges it as the CLI compiles it:
   `audit kit-forge <mixin> --base-image <kit> --library --compare <package>`.
   The changelog is not written by hand: the `feat(sandbox): …` commit makes
   the entry release-plz writes.
7. Add or extend a live test in the `tests/live.rs` of that crate and run
   `just test-live` (background); report what it proved and what it did not.
