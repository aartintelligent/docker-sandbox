---
name: sbx-live
description: Run anything against the real sbx daemon of the user without damaging it — a full audit, a suite replay, the live tests or an example — with its state recorded before and checked after. Use before any command that creates sandboxes, images, rules, secrets or MCP servers.
argument-hint: the run, e.g. "just test-live" or "just scenarios agents"
disable-model-invocation: true
---

# Running against the daemon of the user

The daemon is the user's own: what a run leaves behind stays on their machine.
The procedure is in `audit/README.md`, section "The daemon of the user"; this
skill is its checklist. Run: $ARGUMENTS

## Before

- Run as the user owning the daemon, never as root.
- `just state-save` records sandboxes, images (tagged or not), rules,
  settings, and the secrets and MCP servers `sbx` lists.

## During

- Never call `/daemon/reset`, `/daemon/shutdown` or `POST /policy/network/setup`,
  nor `sbx reset`; never read or change the stored secrets of the user. Dummy
  values only.
- Run anything that may take more than a few minutes (`just audit`,
  `just upgrade`, `just test-live`, the examples) in the background with its
  output in a file: a foreground command killed at its time limit leaves
  sandboxes, MCP registrations and secrets behind. To stop a run, send it
  SIGINT (`timeout -s INT`), never SIGKILL: it cleans up on SIGINT.

## After

- `just state-check`. It fails when something differs and names it.
- References of assembled kit images (`sandbox-kit-assembled:…`) gone without
  any request are the daemon's own collector: `state-check` names its runs.
  The kit images stay, untagged; list images with `?all=true`, never without.
- Leftovers of a run are named after it: sandboxes `audit-*`,
  `docker-sandbox-*` and `example-*`, MCP servers `audit-*` and
  `example-*-tools`, secrets `mcp:<server>:…` and their `:endpoint` entry,
  and custom secrets set by the CLI for a removed sandbox
  (`sbx secret rm --placeholder <placeholder> --sandbox <name> -f`).
  Remove them, then run `just state-check` again until it passes.
- Report to the user what the run left and what was cleaned, faithfully.
