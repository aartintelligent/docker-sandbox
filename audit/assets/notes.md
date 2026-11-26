## Observed behaviors (not inferable from the captures)

Measured on sbx v0.54.0 (api_version 0.52.0), checked again on v0.54.1 (same
API) by the live tests and the `shell-network` example. Check them again after
every `sbx` upgrade.

### Life cycle and sessions

- **A sandbox only stays running while a session is open.** `GET /sandbox/{name}/session`
  is a stream that never sends data; it keeps the sandbox running. About **30 s**
  after the last session is closed, the sandbox becomes `stopped` (measured: 32 s;
  a session held for 45 s kept it running).
- Before every action, the CLI chains `GET /sandbox/{name}/session` (open for the
  whole command) → `GET /sandbox/{name}` → `POST /sandbox/{name}/credentials` →
  `POST /sandbox/{name}/start` (idempotent when already running). A manager that
  drives long-lived sandboxes must **hold its own session**.
- Routes acting inside the VM answer `409 {"message":"sandbox \"X\" is not running"}`
  when the sandbox is stopped (`files`, `mounts`, port publication…).
- `DELETE /sandbox/{name}` expects `expected_id` (the id returned at creation) and
  `force=true|false` in the query string.

### Creation

- `POST /sandbox` (`Accept: application/x-ndjson`) answers with a stream:
  `{"progress": "…"}` lines, then one `{"result": {...}}` line.
- The body holds `kit_artifacts` (about 17 KB per kit), **assembled by the CLI**
  from the kit references (OCI resolution, manifests, capability selection). The
  daemon only builds the image (`sandbox-kit-assembled:<hash>`). Replaying a
  captured `kit_artifacts` works (tested).
- Without a workspace path there is no bind mount: the agent works in the
  container file system.
- `ssh_agent_socket_path` and `bindings_path` are **host** paths provided by the
  client. For the SSH agent, the daemon checks the protocol (it sends
  `SSH2_AGENTC_REQUEST_IDENTITIES`) and keeps the previous socket when the answer is
  invalid. Both fields are optional.
- A sandbox does not mix kit formats: a v3 kit (`# syntax=docker/sandbox-kit:3`,
  built by BuildKit) only accepts v3 mixins.

### Custom kits (mixins)

Tested end to end with `assets/kits/audit-mixin-v3` (a v3 mixin with a
`lifecycle@1` install command), then replayed **without the CLI**.

- **Format.** A v3 kit is a directory whose `spec.yaml` starts with
  `# syntax=docker/sandbox-kit:3` (`schemaVersion: "3"`, `kind: mixin|workload`,
  `capabilities: [...]`). A sandbox does not mix v3 with v1/v2 kits. In a mixin,
  `agent-context@1` requires a `filename` with an explicit directory.
- **No build for a declarative mixin.** No builder (`sbx kit builder status` stays
  "not created"), no BuildKit, no host Docker: the CLI packs the directory and the
  daemon assembles it.
- **Loading the source**: `POST /docker/images/load` (`application/octet-stream`),
  whose body is a deterministic **OCI image layout archive** (1970 timestamps):
  - `oci-layout`, `index.json` (annotation
    `org.opencontainers.image.ref.name = sbx-kit-src:spec-<hash>`);
  - a manifest annotated with `vnd.docker.sandbox.kit.descriptor` (the spec as
    JSON), `.capabilities`, `.schema-version` and `.built-by`;
  - a minimal config and **one layer** holding `usr/share/sandbox/kit/spec/kit.yaml`.

  The daemon stores the image as `sbx-kit-src:spec-<hash>`.
- **Creation**: `POST /sandbox` with `kits` = [base kit reference, mixin path] and
  one `kit_artifacts` entry per kit. For the mixin: `manifest` (including
  `template: sbx-kit-src:spec-<hash>`), `v3` (raw spec), `v3Effective` (normalized
  spec, commands as arrays), `v3Selection`, `commands` (install commands
  **wrapped by the CLI**: `env -i PATH=… HTTP_PROXY=… …`), `v3Digest` (digest of
  the loaded OCI manifest), `v3Reference` and `kitUsage` (`source_type: local-dir`).
- **The daemon assembles** a `sandbox-kit-assembled:<hash>` image (plus the
  `-kit-0`, `-kit-1` and `-template` tags), runs the `install` commands (logged in
  `/var/log/sbx-kits`), then caches the image: recreating the same composition does
  not read the source again.
- **When the source is missing** (and nothing is cached), the answer is still
  **HTTP 200**: the failure arrives in the ndjson stream as an
  `{"error": "… image is not cached locally: image \"sbx-kit-src:spec-…\" …"}`
  line. A client must read the stream until the `result` or the `error`.
- **Adding a mixin to a running sandbox** (`sbx kit add`): `POST /docker/images/load`,
  then **`POST /sandbox/{name}/swap-container`**
  `{docker_name_override, kit_artifacts, kit_refs}` →
  `201 {committed_image_id, container_id, docker_name}`. The daemon commits the
  current container to an image (`sandboxes-swap/<name>:<tag>`, removed with the
  sandbox) and recreates it with the longer kit list: same sandbox id, changes kept.
- **Replay without the CLI, verified**: an OCI archive and a `kit_artifacts`
  captured once → `POST /docker/images/load`, then `POST /sandbox` → the sandbox is
  created and the install command runs.
- `kit inspect --json` only gives a summary (8 fields): the full `kit_artifact` is
  only visible by capturing the CLI traffic.
- **Source directories are not needed for a replay**: the daemon resolves each
  mixin by its `sbx-kit-src:spec-<hash>` image, not by the `v3Reference` path
  (tested with two mixins, one of whose directories had been deleted).
- **Network rules of a mixin** (`assets/kits/audit-mixin-net`, a
  `network-policy@1` with `runtime.allow` and `runtime.deny`): the CLI copies
  them into the `caps.network` of the artifact and sends no policy request; the
  **daemon** creates them at `POST /sandbox`, as a policy of its own (`SOURCE
  kit` in `sbx policy ls`) holding an allow rule `kit:<sandbox>` (provenance
  `provisioned`) and a deny rule `kit:<sandbox>:deny`, both scoped to the
  sandbox and not editable, and removes them with the sandbox. The kit lock of
  the sandbox records them as `networkRuntimeAllow`, `networkRuntimeDeny` and
  their HTTP forms (`ANY <host>/**`). A package replayed through the socket alone
  gets the same rules (`create-x-from-package`, `policy-ls-x`).
- **Install-phase network rules in a mixin** (`install.allow`) are accepted on
  this daemon and recorded in the kit lock as `networkInstallAllow`; the CLI
  puts no `caps` in the artifact, the daemon reads the descriptor itself. The
  binary refuses them on another backend ("accepts only from the workload
  Kit").
- **Credentials of a mixin** (`assets/kits/audit-mixin-cred`, a
  `credential@1` with `apiKey.inject`): the CLI refuses a credential whose
  inject domain is not in the runtime allow list of the kit ("inject domain …
  is not in the network policy's runtime allow list"). A service `sbx` does not
  know is accepted. Without a binding in the bindings file of the host
  (`~/.config/sbx/credentials.yaml`), the credential is not injected: the CLI
  says so and creates the sandbox, even for a credential not marked
  `optional` (warnings only). A creation replayed through the socket answers
  `"withheld": ["<service>"]` in its `result`. The kit lock records
  `credentialsRuntime`. The artifact carries `credentials` and
  `optionalCredentials`; `kit inspect` shows the descriptor as the CLI reads
  it (`permissions.network`, `credentials`).
- **MCP from a mixin**: no dedicated capability is needed. Install and startup
  commands given `env: [MCP_GATEWAY_URL, MCP_SENTINEL_TOKEN_NAME]` get the
  gateway of the sandbox (`http://mcp-gateway.docker.internal/mcp`) and its
  sentinel token name (`assets/kits/audit-mixin-mcp`). Startup commands run on
  `POST /sandbox/{name}/start`: a sandbox created through the socket runs them
  once it is started, as `Sandbox::ensure_running` does.
- **Adding permissions to an existing sandbox**: `sbx kit add` asks to approve
  the permissions a mixin adds ("this change requests permissions beyond what
  this sandbox was granted", refused with `--no-input`). The daemon asks for
  nothing: `POST /sandbox/{name}/swap-container` with the network mixin applies
  its allow and deny rules (`swap-y-net`). The approval is the CLI's alone. A
  swap that would change the credentials of the sandbox is refused with `409`
  ("binding-gated credential set, which the running proxy cannot pick up live;
  remove and recreate the sandbox instead", `swap-y-caps`).
- **A mixin forged without the CLI** (`audit kit-forge`, mixins
  `assets/kits/audit-mixin-json*`, the base kit artifact taken from a package
  the CLI compiled): the daemon accepts a source image and a kit artifact
  written from the kit directory alone. The forged artifact is the one the CLI
  writes but for the name of the source image and its digest. The daemon does
  not check that name (`sbx-kit-src:spec-<hash>` with a hash of our own), the
  `built-by` annotation, nor the form of `kit.yaml` in the layer (the source
  text as is). What it needs, measured by leaving fields out
  (`create-f…-forged`): `manifest` (with `template`, the source image), `v3`
  (the descriptor, from which it derives the permissions and the network
  rules: `caps` is not needed) and `commands` (the install commands it runs).
  `v3Effective`, `v3Selection`, `v3Digest`, `v3Reference`, `kitUsage` and
  `caps` can be left out. **Without `commands` the sandbox is created and no
  install command runs, without any error.** Startup commands run on `POST
  /sandbox/{name}/start`, as for a compiled package.
- **Tool**: `audit kit-compile <dir>… [--base shell] [--kit-arg k=v]` writes
  `target/audit/kits/<name>/` (`kit.json`, `create-body.json` with
  `"name": "{{name}}"`, `images/NN.oci.tar`), cleans the daemon up, then verifies
  the package by replaying it through the socket alone. A package is tied to the
  sbx version (`compiled_with`): compile it again after an upgrade.

### Execution

- `POST /sandbox/{name}/exec`: synchronous, JSON answer `{exit_code, stdout, stderr}`.
  Fields: `cmd`, `user` (e.g. `"0"` for root), `working_dir`, `env`. The CLI never
  sends `env`; the daemon applies it (`exec-env`).
- `POST /sandbox/{name}/exec/attach`: `Connection: Upgrade`, `Upgrade: tcp` → `101`,
  then an `application/vnd.docker.raw-stream` stream (Docker stdcopy multiplexing).
  The execution id is in the **`Sandboxes-Exec-Id`** response header; the exit code
  is then read with `GET /sandbox/{name}/exec/{exec_id}` → `{exit_code, id, running}`.
- `POST /sandbox/{name}/exec/{exec_id}/resize?h=<rows>&w=<columns>`: resizes the
  terminal; `409 exec_not_running` once the command has ended.
- Body of `exec/attach`: `cmd`, `user`, `working_dir`, `env` (object), `tty`
  (boolean), `terminal_size` (`{height, width}`). Without a terminal, the output is
  multiplexed in the stdcopy format (stdout = 1, stderr = 2, daemon errors = 3);
  with a terminal, it is raw. Closing the write side of the connection closes the
  standard input of the process.
- `POST /sandbox/{name}/agent/session`: same mechanism as `exec/attach` (101 +
  `Sandboxes-Exec-Id`), starts the agent of the kit. The CLI forwards `SBX_HOST_*`
  variables of the host graphical session (masked in the captures). It never sends
  `terminal_size`; the daemon applies it (`agent-session-sized`: `stty size`
  prints `33 111`).
- `POST /sandbox/{name}/ssh`: `Connection: Upgrade`, **`Upgrade: ssh`** → a tunnel
  to an SSH server (banner `SSH-2.0-Go`). Measured by the `shell-ssh` example
  with the OpenSSH client behind a `ProxyCommand` relaying the tunnel: the
  server authenticates any user with the `none` method, `root` included, and
  offers an ed25519 host key; the socket is the only boundary. `sbx setup ssh`
  wires `sbx ssh proxy HOST` as that `ProxyCommand` in the configuration of
  the user (`ssh <sandbox>.sbx`).

### Files

- `PUT /sandbox/{name}/files?path=<directory>`: `application/x-tar` body (chunked),
  extracted into the target directory; files are created as root.
- `GET /sandbox/{name}/files?path=<path>`: answers with a tar archive.

### Secrets

- Stored on the client side (keychain, `~/.config/sbx/credentials.yaml`) and pushed
  with `POST /sandbox/{name}/credentials` (`{}` when there is nothing to push). A
  `custom_secrets` push **replaces** the previous list of the sandbox (verified:
  `[A]` then `[B]` → the daemon only keeps `B`). Values travel **in clear text**
  and can never be read back through the socket (`/daemon/debug/state` shows
  `"value": "***"`).
- Measured by the live tests, with dummy values and a public echo service: a
  custom secret pushed through the socket reaches a request of the sandbox
  that carries its placeholder in a header, over HTTP as over HTTPS; the proxy
  writes the value in place of the placeholder, and the sandbox never holds
  it: a search of its files run as root (`/proc`, `/sys` and `/dev` aside)
  finds no value. A secret goes to its own targets only: the placeholder of a
  secret for another host reaches the echo service unchanged. The secret
  stays through a stop and a start without being pushed again.
  An empty update (`{}`, as before every operation of the CLI) leaves the
  custom secrets alone; `"custom_secrets": []` removes them all.
- The proxy leaves the placeholder unchanged in a request to a server of the
  host (`host.docker.internal`). A custom secret pushed through the socket, or
  set by `sbx secret set-custom --sandbox`, puts no variable named after its
  `env_name` in the environment of an execution, of a login shell or of the
  profile files.
- The secrets a program pushes live in the daemon only and go with the
  sandbox. `sbx secret set-custom --sandbox` also records the secret in the
  store of the CLI, where it stays once the sandbox is removed:
  `sbx secret rm --placeholder <placeholder> --sandbox <name> -f` removes it
  (the placeholder alone removes nothing).
- Service credentials, measured by the `shell-secrets` example with dummy
  values: every sandbox is created with the placeholder
  `GH_TOKEN=gho_sbxproxymanaged000…` in its environment. A credential pushed
  for a sandbox (`credential_values` and `credential_sources`) shows in the
  `credential_sources` of its description (`{"source": …, "type": "env"}`)
  and leaves the environment unchanged; an empty value with the source removed
  takes it back.
- `POST /oauth/{service}/reload`: `400 unsupported oauth service` for an unknown
  service.

### Ports

- `POST /sandbox/{name}/ports`: an array `[{host_ip?, host_port, protocol?, sandbox_port}]`;
  refused when the sandbox is stopped.
- `POST /sandbox/{name}/ports/replace-desired`: `{old, new}`; replaces the
  **desired** configuration (applied again at start) without touching the ports
  published right now.
- A UDP binding (`"protocol": "udp"`) is published on a running sandbox and
  unpublished as a TCP one is (measured by the `shell-lifecycle` example).

### Miscellaneous

- The daemon removes assembled kit images on its own, without any request:
  its log reads `assembled kit image gc` with the `removed` and `kept` counts.
  Once observed about eight minutes after the last sandbox went away, it
  removed the `sandbox-kit-assembled:…` tags and kept the kit images they
  pointed to (`docker.io/sbx/claude`, `docker.io/sbx/shell`), untagged:
  `GET /docker/images` lists tagged images only, `?all=true` lists them too. A
  reference that disappears during an audit or a live test is not necessarily
  removed by it.
- `POST`/`DELETE /sandbox/{name}/mounts` `{host_path}`: manages a host path
  **permission** ("Allowed …" / "Removed explicit host-path permission …"), not an
  immediate mount. Measured by the `shell-policy` example: the path must exist
  on the host (`400 resolve mount source: lstat …: no such file or
  directory`), and a grant is listed in the `runtime_mounts` of the sandbox
  until it is revoked.
- `PUT /sandbox/{name}/working-directory` `{working_dir}`: records the working
  directory.
- Templates (`templates` suite): `POST /sandbox/{name}/save` `{tag}` answers
  `201 {image_id, tag}`, the tag normalized (`docker.io/library/<tag>`).
  `sbx create -t IMAGE --pull never` sends `"template": "IMAGE"` and
  `"pull_policy": "never"` in `POST /sandbox`; the new sandbox holds the
  changes saved with the image (`create-b-from-template`, `exec-b-saved`).
- `POST /sandbox/{name}/kits` `{kit_artifact: …}`: never called by the v0.54 CLI.
  Fed with the mixin artifact of a compiled package (its source image loaded
  first), it answers `200 {"message": "kit \"…\" added to sandbox \"…\""}` and
  runs the install commands **in the running container**, which is not recreated;
  the kit list of the sandbox (`com.docker.sandbox.kits` label) is not updated
  (`kits-add-artifact`). `swap-container` is the persistent alternative.
- `GET /sandbox/{name}/debug/vm?route=…`: relays an allow-listed set of internal
  VM diagnostics; `/metrics` (Prometheus format) is allowed. `POST` and `DELETE`
  have their own allow-lists, which do not include `/metrics` (400).
- `GET /user-prompts`: stream of approval requests (starts with
  `{"action":"synced"}`); answered with `POST /user-prompts/{id}/response` `{option_id}`.
  A connection denied by a rule is answered `403` by the proxy and leaves no
  prompt (`exec-blocked`, `user-prompts-after-block`). Prompts are presumably
  raised when no rule decides, which the default `default-allow-all` rule never
  lets happen: no prompt payload has been observed.
- Static MCP sets (`create-s-static-mcp`): `sbx create --static-mcp NAME` sends
  `mcp: {"servers": ["NAME"], "static": true}`; the gateway then lists
  `servers` and `static: true`, and still accepts unloading (`204`) and loading
  (`200`) servers through the socket.
- `servers` of `GET /sandbox/{name}/mcp/gateway` lists the servers the daemon
  loaded, and also those the sandbox loaded itself through the `mcp-add` tool
  of its gateway. The side of the gateway inside the sandbox is outside the
  socket and the audit; the `mcp-*` examples show it (`examples/README.md`).
- `POST /mcp/registration/check` accepts a minimal `spec` (`Name`, `Type`, and
  `Command` or `URL`) instead of the whole structure the CLI sends, and answers
  `{allowed, name}`; nothing is registered.
- `POST /docker/images/create`: a pull the registry refuses fails with an HTTP
  status (`403 {"code": "registry_access_denied"}`), not an in-band line.
- Global network rules (`policy-allow-global`, `policy-rm-global`): the same
  bodies as sandbox-scoped ones, without `sandbox_id`.
- `POST /policy/network/rules` knows six actions: `allow`, `deny`, `http-allow`,
  `http-deny` (`methods`, `path`), `remove-resource` and `remove-id` (`{"action":
  "remove-id", "id": …}`, without `resources`). A change that fails — unknown
  action, unknown rule identifier — is still answered **HTTP 200**: its entry in
  `results` carries an `error`. An empty `actions` list is a 400. A `remove-id`
  of a sandbox-scoped rule needs its `sandbox_id`: without it the daemon only
  looks in the global policy (`rule … exists in sandbox …, not in the global
  policy`, measured by the live tests of the library).

### Error answers (`errors` suite)

- An unknown setting is a `404` for `GET`, `POST` and `DELETE`. An unknown log
  category or level is a `400`; levels are `debug`, `info`, `warn`, `error`.
- A sandbox that does not exist is a `404` for start, stop, deletion, save,
  session, credentials, exec, ports and MCP gateway, sometimes with the code
  `sandbox_not_found`. Deleting with another `expected_id` is a
  `409 sandbox_identity_changed`.
- A stopped sandbox: `exec`, `exec/attach` and both `files` routes answer `409`.
  The SSH tunnel still opens (`101`, banner) and the sandbox stays stopped; the
  working directory is recorded; an **agent session starts the sandbox**.
- In a running sandbox: an upload into a missing directory is a `500` (`tar
  extract failed`); a missing path to download is a `404`; an empty `cmd` is a
  `400`; a missing program is a `200` with exit code `127`; an unknown execution
  is a `404`; unpublishing a port that is not published is a `200`. An
  execution is forgotten once its end has been reported: inspecting it again
  is a `404` (measured by the `shell-exec` example).
- Images: exporting an unknown image is a `404`; its local metadata is a `200`
  with the defaults; an archive that is not an image is a `500`.
- `POST /policy/network/check` with no target or an unknown type is a `400`
  (`type must be "network" or "http"`); an unknown prompt is a `404`; a
  registration check without `spec` is a `400`.
- `GET /events`: see the dedicated section.
- Errors: `{"message": "…"}`, sometimes with a `code` (`invalid_input`,
  `desired_port_not_found`, `exec_not_running`…) or an `error`.

### Agents: workspaces, clone mode, sessions (`agents` suite)

- Workspaces: the first path is `workspace`, the others are
  `additional_workspaces`, each `{"dir", "read_only"?, "target"?}`. Each is
  mounted at its own path, or at `target`; `:ro` mounts it read-only, and a
  single file held `:ro` inside a writable workspace cannot be written. The
  working directory of the sandbox is the primary workspace. Without a path,
  there is no `workspace` field and no bind mount.
- Clone mode: `"clone": true` with the repository as `workspace`. The host
  repository is mounted read-only at `/run/sandbox/source`; the clone sits at
  the path of the repository, with its history, and the agent commits there.
  After the creation, the CLI reads the setting `clone.legacyTcpRemote`, then
  checks the remote by running `git upload-pack <repository>` in the sandbox
  (`exec/attach`, `GIT_PROTOCOL=version=2`, user `1000`, working directory `/`,
  `expected_id` of the sandbox).
- The remote is host Git configuration, written by the CLI:
  `remote.sandbox-<name>.url` is `/sandbox/<name>`, two refspecs
  (`refs/remotes/sandbox-<name>/*` and `refs/sandboxes/<name>/*`), and
  `remote.sandbox-<name>.uploadpack` runs `sbx upload-pack <name>` with
  `DOCKER_SANDBOXES_API` set to the daemon socket the CLI resolved — the real
  one, not the one it was pointed at, so a fetch bypasses the spy. Removing the
  sandbox removes the remote.
- Agent arguments: the session request carries them as `args`, whether they
  start with a flag (`-c …`, kept after the default command) or a word (`echo
  …`, in place of the default command); the daemon applies the rule. `-e` and
  `-w` of `sbx run` become the `env` and `working_dir` of the session. A
  session opened with `"tty": false` multiplexes its output, standard output
  and error apart, as an execution without a terminal (measured by the
  `shell-exec` example).
- Kit arguments (`--kit-arg name=value`) are resolved by the CLI into the kit
  artifacts: a kit declares each argument with a default, a description and a
  pattern or a list of values (`model` and `reasoningEffort` for `claude`),
  and the resolved values reach the sandbox as the variables the kit names
  (`CLAUDE_MODEL`, `CLAUDE_REASONING_EFFORT`) and through its install command
  (`~/.claude/settings.json`); measured by the `agent-kit-args` example.
- Every session the CLI opens carries `TERM`, `COLORTERM`, `TZ` (a POSIX rule
  without daylight saving time, e.g. `CET-1`), the desktop session of the host
  as `SBX_HOST_*`, `ssh_agent_socket_path` and `working_dir` (empty for the
  default); the start request carries `ssh_agent_socket_path` too.
- At creation: `-e` is `environment`, `-p` is `ports` (`tcp` becomes `tcp4`),
  `--skills` is `skills` (`off`, `readonly`, `readwrite`; absent by default),
  and each `--deny-network` host becomes a sandbox deny rule
  (`POST /policy/network/rules`) right after the creation. `--profile` is
  `profile`; the daemon refuses an unknown one with a `400` (`profile "…" not
  found`), before creating anything.

### Routes known but deliberately not exercised

- `/daemon/reset` (wipes everything) and `/daemon/shutdown` (stops the daemon):
  found in the binary, never probed.
- `POST /policy/network/setup`: changes the global network policy.
- `POST /oauth/{service}/reload` with a real service: it would act on the stored
  OAuth credentials of the user.
