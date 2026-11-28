# audit

Maps what the private `sandboxd` socket of Docker Sandboxes (`sbx`) lets a
program drive, and keeps that map up to date when `sbx` changes. The two crates
of the workspace are built on what it measures: `docker-sandbox` on the routes
and payloads of the daemon, its test fixtures exported from the audit
snapshots, and `docker-sandbox-kit` on the kit artifacts the CLI compiles,
which the `kits` suite compares with the ones it forges.

Everything the audit needs is in this crate, and everything it records is free
of personal data.

## Layout

```text
audit/                    the crate `audit`
  assets/                 embedded in the binary
    suites/*.json         scenario suites, replayed in order
    kits/audit-mixin-v3/  test mixin (also used by the live tests)
    kits/audit-mixin-*/   test mixins of network rules, credentials, MCP and kit-forge
    fixtures/             scenario input files
    notes.md              hand-written observations, inserted into API.md
    probe-routes.txt      extra routes for `probe`
  snapshots/<version>/    one audit per sbx version
    manifest.json         sbx version, date, suite outcomes, contract size
    routes.json           router inventory (binary scan + OPTIONS)
    router-log.json       router patterns the daemon matched during the run
    events.ndjson         GET /events recorded during the run
    captures/<suite>.jsonl  spy capture of each suite
    results/<suite>.json    outcome of each scenario
    api/openapi.json      OpenAPI 3.1 contract
    api/API.md            Markdown reference
```

Scratch output goes to `target/audit/` and the scenario working directory to
`$TMPDIR/docker-sandbox-audit-work`, so that the paths sent to the daemon are the same on
every machine.

## The daemon of the user

Every run below works on the daemon of the user: run it as that user (not
root), on a machine where test sandboxes can be created and removed, and never
call `/daemon/reset`, `/daemon/shutdown` or `POST /policy/network/setup`.
Record the state of the daemon before, and check it after:

```sh
just state-save     # sandboxes, images (tagged or not), rules, settings, secrets, MCP servers
just state-check    # fails when something differs, and names what
```

`audit` and `scenarios` restore the daemon on their own and report what they
could not restore; `just test-live` records and checks the state around the
live tests. The daemon also removes assembled kit images by itself, minutes
after the last sandbox goes away: `state-check` names the runs of that
collector it finds in the daemon log, so that a reference it took is not
blamed on the run.

A sandbox of an agent kit (`claude`, `codex`, …) created through the socket
from a package the CLI compiled or `docker-sandbox-kit` forged gets the credential the
daemon holds for the kit, a subscription of the user for instance: start no
agent in it unless the user asks, since the agent would use their account.
The CLI also pushes the stored credentials of the user into a sandbox before
`exec`, `run`, `cp`, `kit add` and `port publish`: never run those on the
sandbox of an agent kit.

The examples keep the kit images they pull and load as a cache
(`docker.io/sbx/<kit>:latest`, `sbx-kit-src:…`, `sandbox-kit-assembled:…`). To
bring the daemon back to its recorded state after them, remove the images
`state-check` names, derived tags first:

```sh
cargo run -p audit -- raw DELETE "/docker/images/remove?name=<reference, URL-encoded>"
```

## When `sbx` changes

```sh
just upgrade v0.54.0 v0.55.0
```

`upgrade` chains the three steps below and writes the API changes to
`target/audit/compare-<old>-<new>.md`. Step by step:

```sh
cargo run -p audit -- audit
```

`audit` replays the suites against the live daemon and writes
`snapshots/<new version>/`. The daemon is left as it was found: settings changed
during the run are restored, sandboxes and images created by the run are removed,
and anything it cannot restore is reported. The snapshot is only written when
`privacy-check` finds no personal data in it.

Then:

```sh
# what changed in the API
cargo run -p audit -- compare audit/snapshots/<old> audit/snapshots/<new>

# refresh the decoding fixtures of docker-sandbox from the new snapshot; the
# fixtures of exchanges the snapshot no longer has are removed
cargo run -p audit -- fixtures audit/snapshots/<new>

cargo test --workspace
```

Every line of the `compare` report is something the library may have to handle:
new or removed operations, fields, statuses, routes and event kinds. Adapt
`docker-sandbox` (types and routes) until the tests pass, then run the live tests
(`just test-live`).

### What `docker-sandbox-kit` has to follow

`docker-sandbox-kit` does on the client side what the CLI does, without the
CLI: the kit artifacts it forges follow a private contract between the CLI and
the daemon, which a new `sbx` can change without changing a route. Check, before
`SUPPORTED_API` is widened:

1. **The artifacts.** The `kits` suite compares the ones the crate forges, field
   by field, with the ones the new CLI compiles (`kit-forge-library`,
   `kit-forge-library-shell`, in `snapshots/<new>/results/kits.json`). Every
   field is `same` but the `template` and `v3Digest` of the mixin, the name
   and digest of its source image, which differ by design; a field reported
   `differs`, `sbx only` or `forged only` is a change `docker-sandbox-kit/src/forge/`
   has to follow.
2. **The capability types.** The crate places the capabilities of a kit in the
   order the CLI gives them (`RESOLVED` in `forge/artifact.rs`), which decides
   whether the daemon authorizes the credential of a published kit, and skips
   the optional types the CLI does not support (`UNSUPPORTED`). A published kit
   that declares a type outside both is refused ("not one this crate knows the
   CLI to resolve"): find where the CLI places it, in the `v3Effective` of a
   package it compiled (`audit kit-compile`), then add it there. The `kits`
   suite compares the forge with the CLI on the `shell` kit only: compare an
   agent kit too, with `audit kit-compile --base claude` then `audit kit-forge
   --library --compare` on that package. `sbx` v0.55 knows `open-url@1`,
   which v0.54.1 leaves out as unsupported, and five other new types (see
   `assets/notes.md`): at its upgrade, the capabilities of the `claude` kit
   may take another place.
3. **The mixin descriptor.** The crate refuses any field it does not know in a
   mixin. A field the new CLI accepts is measured with a scenario of the `kits`
   suite before `forge/descriptor.rs` accepts it.
4. **The compilation by the CLI.** `KitCompiler` captures `POST /sandbox` and
   `POST /docker/images/load` from the CLI (`CAPTURE_RULES` in
   `compiler/mod.rs`). `just test-live` compiles the package of the live tests
   with it, through `audit kit-compile`: a CLI that sends them otherwise fails
   there.
5. **The registration of MCP servers.** `Mixin::mcp` registers a server in
   Claude Code with the command the claude kit registers its gateway with
   (`claude mcp add … --transport http --scope user --header …`, in
   `forge/mcp.rs`), and in copilot and opencode by a merge with `jq` into the
   file their kit merges its gateway into (`~/.copilot/mcp-config.json` under
   the lock of the kit, `~/.config/opencode/opencode.json`). When the commands
   of a new kit register the gateway otherwise, follow them; `assets/notes.md`
   says what each agent reads ("MCP servers registered in the agent").
6. **For real.** `just test-live` forges `shell` and `claude`, with two of its
   arguments and an MCP server, against the new daemon. The other agent kits are forged by their
   examples (`agent-<agent>`), which start their agent: run them only when the
   maintainer asks.

A forged package records the API it was forged for (`kit.json`), and creating a
sandbox from it logs a warning once the daemon speaks another: forge the
packages again after the upgrade.
The two crates share one version, so the release of the adaptation carries
both.

When a suite fails on the new version, fix the suite first (`sbx <cmd> --help`
tells the new syntax), then replay it alone:

```sh
cargo run -p audit -- scenarios --suite kits
cargo run -p audit -- scenarios --suite sessions --only create-a,exec-env,rm-a
```

A replay restores the daemon like a full audit does: changed settings are reset,
and the sandboxes and images it created are removed.

## Measuring a CLI feature before coding it

What the library sends is measured on the CLI first, never guessed: a flag of
`sbx run` or `sbx create`, a field, a route. The `agents` suite was written
this way.

1. **Read** what the feature does: the Docker Sandboxes documentation and
   `sbx <command> --help` of the installed version.
2. **Write scenarios** that use it, into the suite of its domain or a new one
   (registered in `src/assets.rs`), with the shell kit only. A scenario runs
   `sbx` (or another `program`, e.g. `git` to prepare an input), or sends a
   request; inputs go to `assets/fixtures/` (`{fixtures}`), scratch files to
   `{work}`. Dummy values only.
3. **Replay the suite alone**, out of the snapshot, and read what was sent:

   ```sh
   just state-save
   just scenarios agents
   just requests agents --only create-w-workspaces,run-r-flag-args
   just state-check
   ```

   The outputs of each scenario are in `target/audit/scenarios/<suite>.json`.
   Iterate with `--only` until every scenario says what it is meant to.
4. **Write down** what the captures cannot show (what the sandbox saw, what
   the CLI does on the host, what the daemon refused) in `assets/notes.md`.
5. **Record it**: `just audit --force` writes the snapshot of the installed
   version again, then `just fixtures <version>` exports the fixtures and
   `cargo run -p audit -- spec audit/snapshots/<version>` refreshes `API.md`
   after a later edit of the notes.
6. **Code it** in `docker-sandbox`, or in `docker-sandbox-kit` when it lies in a
   kit artifact or descriptor: types and builders shaped on the measured
   bodies, a test asserting those bodies against the fake daemon, coverage
   kept (`just coverage`). A kit feature is then checked to be forged as the
   CLI compiles it:
   `audit kit-forge <mixin> --base-image <kit> --library --compare <package>`.
   The changelog is not edited: release-plz writes it from the commit message
   (`feat(sandbox): …`).
7. **Check it for real** with a live test (`tests/live.rs` of the crate that
   has the feature, `just test-live`), then the examples that use it.

## Suites

| Suite | Covers |
|---|---|
| `lifecycle` | Discovery commands, two sandboxes, files, ports, network rules (sandbox-scoped, and a global one on a dummy host, removed at once), a dummy custom secret, a remote MCP server, settings, a declarative environment |
| `coverage` | The remaining CLI commands: log level, diagnostics, policy profiles, skills, prune, workspaces, agent sessions, exec options, a local MCP server, a dummy service secret |
| `templates` | Saving a stopped sandbox as a template, loading and removing it |
| `sessions` | Routes and fields the CLI rarely or never sends: VM debug actions, terminal and agent sessions, exec environment, raw ports, working directory, host mounts, held sessions, SSH, image pull failures, and the error answers (validation, unknown resources, stopped sandbox) |
| `kits` | Creations cut short and under a name taken, and the network rules they leave; local kits: validation, creation with a mixin, `kit add`, a compiled package replayed through the socket, and its mixin artifact sent to `POST /sandbox/{name}/kits`; mixins that declare network rules, credentials and MCP commands, created by the CLI, replayed through the socket and swapped into a running sandbox; mixins forged without the CLI (`kit-forge`) on base kits taken from a compiled package or rebuilt from their image through the socket (shell, and claude with kit arguments), with fields left out to find what the daemon requires; an install command that fails |
| `errors` | The error answers the library documents: unknown settings, images, rules and prompts, a sandbox that does not exist, a stopped sandbox, invalid paths, programs and identifiers |
| `agents` | What `sbx create` and `sbx run` send for an agent beyond its kit: several, read-only and relocated workspaces, clone mode and the Git remote it adds on the host, agent arguments, the environment and working directory of a session, skills, environment, ports and deny rules at creation, a governance profile |
| `network` | The network policy as the proxy of a sandbox applies it: the forms of a host a rule applies (and those it accepts without applying), deny against allow for the rules of a sandbox and of its kits, two mixins composed, and the check of the policy, which ignores the rules of a sandbox; dummy hosts under `example.invalid` only |

A scenario is one JSON object. Its action is recognized by its field:

```json
{"name": "exec-tty", "category": "exec", "args": ["exec", "-it", "audit-a", "--", "stty", "size"], "tty": {"rows": 24, "cols": 80}}
{"name": "mcp-add-local", "category": "mcp", "mutates": true, "args": ["mcp", "add", "audit-echo", "--command", "{audit}", "--args", "mcp-echo"]}
{"name": "kit-compile", "category": "kit", "program": "{audit}", "args": ["kit-compile", "{kits}/audit-mixin-v3", "--out", "{work}/kit-package"]}
{"name": "mounts-add", "category": "files", "request": {"method": "POST", "path": "/sandbox/audit-a/mounts", "body": {"host_path": "{fixtures}/mountdir"}}}
{"name": "session-hold", "category": "lifecycle", "request": {"method": "GET", "path": "/sandbox/audit-a/session", "hold": "audit-a"}}
{"name": "session-release", "category": "lifecycle", "release": "audit-a"}
{"name": "ssh-upgrade", "category": "ssh", "request": {"method": "POST", "path": "/sandbox/audit-a/ssh", "upgrade": "ssh", "wait_ms": 2000}}
```

- `args` runs `sbx` (with `--no-input`), or `program` when given; `stdin` feeds it
  and `tty` runs it in a pseudo-terminal.
- `request` is sent straight to the socket through the spy: `body` (JSON) or
  `body_file` (with `replace`, and `pointer` and `wrap` to send part of a JSON
  file under a key), `headers` (e.g. the `accept` of the ndjson stream the CLI
  asks for), `upgrade` and `stdin` for upgraded streams, `wait_ms` for bodies
  that never end or a request cut short, `hold` to keep the connection open
  until a `release` step.
- `mutates` marks steps that change daemon state; `expect_error` marks steps that
  document an error, so that an unexpected success is reported.
- Placeholders: `{work}`, `{fixtures}`, `{kits}`, `{spy}`, `{audit}`.

Never add a manual session to the audit: write it as a scenario, so that the
next audit replays it.

## Personal data

Captures are scrubbed when they are recorded: the home directory, user name and
host name of the machine, e-mail addresses, the host session variables the CLI
forwards (`SBX_HOST_*`) and D-Bus GUIDs are replaced with stable placeholders
(`/home/user`, `user`, `host`, `user@example.com`, `<private>`). Secret values
are redacted by the spy, and the suites only use dummy secrets.

Add any other private value (a Docker account name, for instance) with
`--private <value>` or `DOCKER_SANDBOX_AUDIT_PRIVATE`. Check files at any time with:

```sh
cargo run -p audit -- privacy-check            # snapshots, fixtures, published crates
cargo run -p audit -- scrub <in> <out>         # clean an external capture
```

## Other commands

| Command | Purpose |
|---|---|
| `spec <snapshot>` | Regenerates `api/` (after editing `assets/notes.md`, for instance) |
| `routes` | Router inventory alone |
| `router-log` | Router patterns of the daemon log |
| `probe` | `GET`/`OPTIONS` on known and candidate routes |
| `perms` | Socket permissions and daemon identity |
| `spy` | Interactive capture: point `DOCKER_SANDBOXES_API` at its socket |
| `summarize <capture>` | Route catalog and scenario matrix of a capture |
| `requests <capture>` | What each scenario of a capture sent, bodies included (`just requests`) |
| `state` | Records the state of the daemon (`--save`), checks it against the record (`--check`) |
| `kit-compile <dir>…` | Kit package replayable through the socket alone, compiled by the `KitCompiler` of `docker-sandbox-kit` |
| `kit-forge <dir> --base-package <pkg>` or `--base-image <ref>` | Experiment: the package of a v3 mixin written without the CLI, its base kit taken from a compiled package or rebuilt from its image through the socket (`--base-arg name=value` for its arguments, `--omit`, `--omit-base` fields, `--compare` with the CLI); `--library` forges it with the `KitForge` of `docker-sandbox-kit` instead, and `--base-agent <agent>` with it takes the kit of an agent as `KitForge::agent` resolves it, its shortcut on the daemon followed |
| `raw`, `replay` | Arbitrary or captured requests |

Destructive daemon routes (`/daemon/reset`, `/daemon/shutdown`) and the global
network policy setup are never called.
