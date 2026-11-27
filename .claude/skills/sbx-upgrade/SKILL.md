---
name: sbx-upgrade
description: Bring docker-sandbox and docker-sandbox-kit up to a newly installed sbx version — audit the daemon, read the API changes and the kit artifacts, refresh the fixtures, adapt both crates, run the live tests. Use when sbx was upgraded on the machine.
argument-hint: <previous version> <new version>, e.g. v0.54.0 v0.55.0
disable-model-invocation: true
---

# Upgrading to a new sbx version

The procedure is `audit/README.md`, sections "The daemon of the user" and
"When `sbx` changes", with its checklist "What `docker-sandbox-kit` has to
follow"; read them first, and `AGENTS.md` for the conventions.
Versions, previous then new: $ARGUMENTS

1. Check that `sbx version` is the new version and that
   `audit/snapshots/<previous>/` exists.
2. Following the `sbx-live` skill: `just state-save`, then
   `just upgrade <previous> <new>` in the background, then `just state-check`.
3. When a suite fails, fix the suite first (`sbx <command> --help` gives the
   new syntax), replay it alone (`just scenarios <suite> --only …`), then run
   the upgrade again.
4. Read `target/audit/compare-<previous>-<new>.md`: every line is something
   `docker-sandbox` may have to handle. Then go through the checklist of
   `docker-sandbox-kit`: the artifacts the `kit-forge-library` and
   `kit-forge-library-shell` scenarios printed
   (`audit/snapshots/<new>/results/kits.json`), where every field is `same` as
   the CLI's but the `template` and `v3Digest` of the mixin; the capability
   types of the published kits; the fields of a mixin; what `KitCompiler`
   captures. Summarize both for the maintainer, and propose any public API
   change before making it.
5. Adapt `docker-sandbox` and `docker-sandbox-kit` until `just ci` passes;
   widen `SUPPORTED_API` only now that the new version is audited. The two
   crates share one version and are released together. The changelog is not
   written by hand: release-plz derives it from the Conventional Commits
   (`CONTRIBUTING.md`).
6. `just test-live` in the background: it runs the live tests of both crates,
   compiles the package of those of `docker-sandbox` with `KitCompiler` and
   forges `shell` and `claude` with `KitForge`. Report what passed, what
   failed, and what the state check said. The examples of the other agent kits
   start their agent: run them only when the maintainer asks.
