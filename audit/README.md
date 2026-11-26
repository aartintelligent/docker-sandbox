# audit

Maps what the private `sandboxd` socket of Docker Sandboxes (`sbx`) lets a
program drive, and keeps that map up to date when `sbx` changes. The library of
the workspace, `docker-sandbox`, is built on what it measures, and its test
fixtures are exported from the audit snapshots.

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
6. **Code it** in `docker-sandbox`: types and builders shaped on the measured
   bodies, a test asserting those bodies against the fake daemon, coverage
   kept (`just coverage`). The changelog is not edited: release-plz writes it
   from the commit message (`feat(sandbox): …`).
7. **Check it for real** with a live test (`tests/live.rs`, `just test-live`),
   then the examples that use it.

## Suites

| Suite | Covers |
|---|---|
| `lifecycle` | Discovery commands, two sandboxes, files, ports, network rules (sandbox-scoped, and a global one on a dummy host, removed at once), a dummy custom secret, a remote MCP server, settings, a declarative environment |
| `coverage` | The remaining CLI commands: log level, diagnostics, policy profiles, skills, prune, workspaces, agent sessions, exec options, a local MCP server, a dummy service secret |
| `templates` | Saving a stopped sandbox as a template, loading and removing it |
| `sessions` | Routes and fields the CLI rarely or never sends: VM debug actions, terminal and agent sessions, exec environment, raw ports, working directory, host mounts, held sessions, SSH, image pull failures, and the error answers (validation, unknown resources, stopped sandbox) |
| `kits` | Local kits: validation, creation with a mixin, `kit add`, a compiled package replayed through the socket, and its mixin artifact sent to `POST /sandbox/{name}/kits`; mixins that declare network rules, credentials and MCP commands, created by the CLI, replayed through the socket and swapped into a running sandbox; mixins forged without the CLI (`kit-forge`), with fields left out to find what the daemon requires |
| `errors` | The error answers the library documents: unknown settings, images, rules and prompts, a sandbox that does not exist, a stopped sandbox, invalid paths, programs and identifiers |
| `agents` | What `sbx create` and `sbx run` send for an agent beyond its kit: several, read-only and relocated workspaces, clone mode and the Git remote it adds on the host, agent arguments, the environment and working directory of a session, skills, environment, ports and deny rules at creation, a governance profile |

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
  file under a key), `upgrade` and `stdin` for upgraded streams, `wait_ms` for
  bodies that never end, `hold` to keep the connection open until a `release`
  step.
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
cargo run -p audit -- privacy-check            # snapshots, fixtures, published crate
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
| `kit-compile <dir>…` | Kit package replayable through the socket alone |
| `kit-forge <dir> --base-package <pkg>` | Experiment: the package of a v3 mixin written without the CLI (`--omit` fields, `--compare` with the CLI) |
| `raw`, `replay` | Arbitrary or captured requests |

Destructive daemon routes (`/daemon/reset`, `/daemon/shutdown`) and the global
network policy setup are never called.
