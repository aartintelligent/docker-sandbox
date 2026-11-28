# Examples

Each example is a crate of its own, so that its dependencies are clear, and
the examples form a workspace separate from the library, as in axum. Run one
from the repository root:

```sh
just example base
just example agent-claude-mcp
# or
cargo run --manifest-path examples/Cargo.toml -p example-shell-exec
```

Every example talks to the daemon of the current user, creates the sandboxes
it needs and deletes them when it ends, also on an error or Ctrl-C. Run them as
the user owning the daemon, with the `sbx` CLI installed: the first run of a
kit compiles it once with the CLI (see [`common/src/kit.rs`](common/src/kit.rs))
and caches the package under `examples/target/kits/`. The kit images pulled on
that occasion stay on the daemon, as the cache of the next sandboxes.
The agent examples, [`shell-forge`](shell-forge/src/main.rs),
[`agent-kit-forge`](agent-kit-forge/src/main.rs) and
[`blueprint`](blueprint/src/main.rs) need no CLI for their kits:
they forge their packages through the socket alone (`KitForge`, from
`docker-sandbox-kit`), under `examples/target/forged/`, at every run. The `-mcp`
variants still register their MCP server with `sbx mcp add`.

When a kit declares a credential the daemon does not authorize, the creation
of a sandbox reports it as withheld, and the examples print it: the daemon
gives the sandbox none of its values. The credential of a published kit is
authorized when its artifact is the one the CLI resolves, as with the packages
compiled by the CLI and those `KitForge` forges.

## The basics

| Example | Shows |
|---|---|
| [`base`](base/src/main.rs) | Connect, list sandboxes and images, create a sandbox from a kit package, run a command, delete it |
| [`daemon`](daemon/src/main.rs) | A client configured before it connects (socket, deadlines, version policy); what a program reads to watch the daemon: health, settings changed from their default, log levels, disk usage, network policy, internal state and diagnostics; a setting and a log level changed then restored; a sandbox created with CPU and memory limits and the metrics of its VM |
| [`images`](images/src/main.rs) | The images of the daemon: the list, a pull followed as it happens then waited for, the details and local metadata of an image, what an unknown image gives. The tag it adds is removed |

## Shell sandboxes

| Example | Shows |
|---|---|
| [`shell-lifecycle`](shell-lifecycle/src/main.rs) | The life of a sandbox step by step: a creation refused for an unknown profile, a streamed creation with a port and options, typed errors (not found, conflict), sessions, start and stop, desired ports replaced, a UDP port, the working directory, a deletion guarded by the identifier |
| [`shell-exec`](shell-exec/src/main.rs) | Commands: outputs and exit codes, `check`, users, working directory, environment, per-command deadlines, standard input, an execution followed by its identifier, the agent of the kit without a terminal |
| [`shell-files`](shell-files/src/main.rs) | Files: a configuration file, an executable script, whole directories in both directions, tar archives as they are |
| [`shell-network`](shell-network/src/main.rs) | Network policy of one sandbox: denied hosts and the proxy answer, HTTP rules, checks, activity log |
| [`shell-policy`](shell-policy/src/main.rs) | The policy beyond one sandbox: a global rule added, checked and removed by identifier, failures inside a success, hosts a rule cannot name refused before anything is sent (`HostPattern`), sandbox host and HTTP rules removed through the sandbox, the reload of the policy, host path permissions, approval prompts. The global policy ends as it began |
| [`shell-ports`](shell-ports/src/main.rs) | A web server in the sandbox (`serve` of [`sandbox-tools`](sandbox-tools/src/main.rs)), published on a port of the host and reached from it |
| [`shell-secrets`](shell-secrets/src/main.rs) | Secrets pushed through the socket alone, with dummy values. Custom secrets: the proxy writes the value of a secret into a request to its host and leaves the placeholder of another, the sandbox holds no value, the secrets survive a restart and go with an empty list (checked from inside with `get` of `sandbox-tools` and the public echo service `postman-echo.com`); one is handed over as a `secrecy::SecretString`. Service credentials: recorded by the daemon, never in the sandbox. The answer of the daemon to each update, and the credentials it withholds |
| [`shell-ssh`](shell-ssh/src/main.rs) | SSH through the socket: the tunnel of the library relayed to the SSH client of the system as its `ProxyCommand`, with no key and nothing written to `~/.ssh`. The server accepts any user, `root` included: the socket is the boundary |
| [`shell-clone`](shell-clone/src/main.rs) | Clone mode: the sandbox works on a private clone of a repository, the source read-only; the agent commits there, and the host fetches the commits with `git fetch` through a relay of `git upload-pack` in the sandbox |
| [`shell-templates`](shell-templates/src/main.rs) | A sandbox saved as an image, a new sandbox started from it with its changes (`CreateRequest::with_template`, from a stored request), the image exported to a file, removed and loaded back |
| [`shell-kits`](shell-kits/src/main.rs) | A mixin kit of the examples ([`example-mixin`](shell-kits/example-mixin/spec.yaml)) added to running sandboxes: in place (the kit list stays as it was), by a swap of the container (work in progress kept, kit list updated), then both by hand from the parts of the package |
| [`shell-forge`](shell-forge/src/main.rs) | A kit package forged through the socket alone, without the `sbx` CLI: the shell kit rebuilt from its published image, a mixin built in Rust (an install command given a variable of the sandbox, a startup command, a host allowed and one denied) and one read from its directory ([`json-mixin`](shell-forge/json-mixin/spec.yaml), JSON). What a forged mixin may not declare, refused with its reason; the package built in memory, its images too, and assembled again by hand from its request and its images (`KitPackage::in_memory`); then both mixins run in the sandbox, the network rules of the first are applied by the daemon and removed with the sandbox, and the package is saved, its base kit composed again without pulling its image |
| [`shell-interactive`](shell-interactive/src/main.rs) | An interactive shell relayed to the local terminal, window resizing included |
| [`shell-events`](shell-events/src/main.rs) | The daemon event stream: the replay of recent state up to its sync marker, then live events while a sandbox is created, used and deleted |

## MCP

The MCP gateway of a sandbox, checked from where its agent uses it: inside the
sandbox. Each example runs [`mcp-client`](mcp-client/src/main.rs), a minimal
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) client over
streamable HTTP, in a shell sandbox against
`http://mcp-gateway.docker.internal/mcp`, and removes what it registered and
stored when it ends.

| Example | Shows |
|---|---|
| [`mcp-gateway`](mcp-gateway/src/main.rs) | A local server (`sbx mcp add --command`) loaded through the socket, its tools listed and called from the sandbox, the gateway port refusing the host, the server unloaded |
| [`mcp-remote`](mcp-remote/src/main.rs) | A remote server over streamable HTTP (`--url`) behind a bearer token the gateway takes from the `sbx` secret store: called from the sandbox, which never holds the token, then the token rotated |
| [`mcp-static`](mcp-static/src/main.rs) | A sandbox created with a static MCP set (`CreateRequest::with_static_mcp_servers`): served without a load, and closed to additions from inside |
| [`mcp-dynamic`](mcp-dynamic/src/main.rs) | The gateway's own tools used from inside: `mcp-find` finds a server registered on the host, `mcp-add` loads it, `mcp-exec` calls a tool by name |

The client is built on first use as a static binary for the sandbox
(`cargo build --target <arch>-unknown-linux-musl`), which needs that target:
`rustup target add x86_64-unknown-linux-musl` (or `aarch64-…` for an ARM
sandbox, with a linker for it on macOS); `shell-ports` and `shell-secrets`
build `sandbox-tools` the same way
([`common/src/program.rs`](common/src/program.rs)). The HTTP
server of `mcp-remote` and `mcp-dynamic`
([`common/src/mcp.rs`](common/src/mcp.rs)) listens on a loopback port of the
host, for the gateway only, with a dummy token made for the run.

What they measured on `sbx` v0.54.0:

- The gateway answers inside the sandbox only: its port on the host answers
  `401 Unauthorized`, since the proxy of the sandbox adds the credential.
- Besides the tools of its servers, a gateway offers `mcp-find`, `mcp-add`,
  `mcp-config-set`, `mcp-exec` and `code-mode`. Tools are not prefixed by
  server: `mcp-add` refuses a server whose tool names are already taken.
- A server added from inside with `mcp-add` stays loaded: the socket lists it
  among the `servers` of the gateway, and later sessions offer it, until the
  program unloads it. An agent can thus load any server registered on the
  host; a static gateway offers only `mcp-exec` and `code-mode`, so a runner
  that must fix the MCP servers of a sandbox creates it with a static set.
- `sbx mcp add --url` probes the server: one that answers `401` without OAuth
  metadata is refused, so a header secret is stored before the registration.
  The first connection adds a `mcp:<server>:<placeholder>:endpoint` entry that
  binds the secret to the URL and outlives it; the examples remove both.
- A gateway fills the header when it connects to the server and keeps that
  connection: a rotated secret is used once the server is unloaded and loaded
  again.

## Coding agents

One example per agent `sbx` knows, and an `-mcp` variant that also gives the
agent the MCP server of the examples. Each forges the kit of its agent from
`docker.io/sbx/<agent>:latest` without the `sbx` CLI
([`common/src/kit.rs`](common/src/kit.rs)), runs the agent session in the
local terminal, and deletes the sandbox when the session ends. The library
refuses a kit that declares a capability it does not know the CLI to
resolve, with its name: the order of the capabilities, which the daemon
checks before it authorizes the credential of the kit, would be a guess.

Like `sbx run AGENT [PATH] [-- AGENT_ARGS...]`, an example takes the directory
the agent works on, mounted read-write at the same path, and arguments for the
agent after `--`: options added to its default command, or a prompt in its
place. Without a directory, nothing of the host is mounted: the agent works in
the filesystem of the sandbox (`sbx run` mounts the current directory, which
would be the repository itself here).

```sh
just example agent-claude
just example agent-claude ~/my-project
just example agent-claude ~/my-project -- "explain this project"
```

| Agent | Examples | Credential read from the environment |
|---|---|---|
| Claude Code | [`agent-claude`](agent-claude/src/main.rs), [`agent-claude-mcp`](agent-claude-mcp/src/main.rs) | `ANTHROPIC_API_KEY` |
| Codex | [`agent-codex`](agent-codex/src/main.rs), [`agent-codex-mcp`](agent-codex-mcp/src/main.rs) | `OPENAI_API_KEY` |
| OpenCode | [`agent-opencode`](agent-opencode/src/main.rs), [`agent-opencode-mcp`](agent-opencode-mcp/src/main.rs) | Any of the provider keys below |
| GitHub Copilot | [`agent-copilot`](agent-copilot/src/main.rs), [`agent-copilot-mcp`](agent-copilot-mcp/src/main.rs) | `COPILOT_GITHUB_TOKEN`, `GH_TOKEN` or `GITHUB_TOKEN` |
| Cursor | [`agent-cursor`](agent-cursor/src/main.rs), [`agent-cursor-mcp`](agent-cursor-mcp/src/main.rs) | `CURSOR_API_KEY` |
| Devin | [`agent-devin`](agent-devin/src/main.rs), [`agent-devin-mcp`](agent-devin-mcp/src/main.rs) | `DEVIN_API_KEY` |
| Docker Agent | [`agent-docker-agent`](agent-docker-agent/src/main.rs), [`agent-docker-agent-mcp`](agent-docker-agent-mcp/src/main.rs) | Any of the provider keys below |
| Droid | [`agent-droid`](agent-droid/src/main.rs), [`agent-droid-mcp`](agent-droid-mcp/src/main.rs) | `FACTORY_API_KEY` |
| Kiro | [`agent-kiro`](agent-kiro/src/main.rs), [`agent-kiro-mcp`](agent-kiro-mcp/src/main.rs) | `KIRO_API_KEY`, given to the agent as is: `sbx` knows no service secret for Kiro |

Provider keys: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY` (or
`GOOGLE_API_KEY`), `OPENROUTER_API_KEY`, `GROQ_API_KEY`, `MISTRAL_API_KEY`,
`NEBIUS_API_KEY`, `XAI_API_KEY`.

A key found in the environment is given to the sandbox of the example only,
through its proxy (`ProxiedKey`, see
[`common/src/credentials.rs`](common/src/credentials.rs)): the sandbox is
created with a placeholder in the variable the agent reads, and the proxy
writes the key in its place in the requests to the hosts the kit declares for
its service, `api.anthropic.com` and `mcp-proxy.anthropic.com` for Claude
Code. The agent never sees the key, and it goes away with the sandbox.
Without any key, the agent uses the credential the daemon holds for its kit, a
Claude subscription or a secret stored with `sbx secret set <service>` for
instance, which the proxy writes into its requests; the examples never read
it. A credential the daemon withholds instead is printed at creation. Without
either, the agent offers its own login inside the session: `/login` for Claude
Code, the device flow for Kiro.

The agent session opens as `sbx run` opens it
([`common/src/host.rs`](common/src/host.rs)): with the terminal type
(`TERM`, `COLORTERM`) and the time zone (`TZ`) of the host, its desktop
session as `SBX_HOST_*` variables, through which the sandbox reaches it, and
its SSH agent, unless the `ssh.agentForwardingEnabled` setting is off.

### Kit arguments

[`agent-kit-args`](agent-kit-args/src/main.rs) chooses the model and the
effort of the `claude` kit, the arguments it declares, when the kit is
compiled (`CompileJob::with_arg`, as `sbx create --kit-arg model=…`): the
sandbox gets them as `CLAUDE_MODEL` and `CLAUDE_REASONING_EFFORT`, and in
`~/.claude/settings.json`. It starts no agent and uses no credential.

[`agent-kit-forge`](agent-kit-forge/src/main.rs) makes the same sandbox
without the `sbx` CLI: `KitForge` rebuilds the `claude` kit from its image and
checks the values against what the kit declares (`KitForge::with_arg`). The
daemon authorizes the credential of the kit as for a sandbox the CLI creates.
It also gives Claude Code the MCP server of the examples, served on a loopback
port of the host, through a mixin (`Mixin::mcp`): its install command runs
`claude mcp add` inside the sandbox, so that nothing is registered on the host,
and the token of the server is given at creation, in a variable Claude Code
reads when it connects. The example sends the `initialize` request of Claude
Code from inside the sandbox, to the URL and with the header it registered.
The mixin also registers a stdio server, which the example starts as Claude
Code would, and the example prints the servers in their JSON form. It starts
no agent either.

### An agentic task launched from a spec

[`blueprint`](blueprint/src/main.rs) runs the same task as the `automation`
examples, the `calc` project whose tests fail, from a `SandboxSpec` of
`docker-sandbox-blueprint`: the `claude` kit, the project as its workspace and
a pristine copy read-only, the key of the agent when the environment holds
`ANTHROPIC_API_KEY`, the MCP tools of the examples served by the program with
`rmcp` on the host, and a network refusal. `SandboxSpec::launch` does the rest
in one call (kit, token of the tool server, key, creation, start) and returns
the `Lease` that owns the sandbox. The task is then one command: the agent the
kit declares (`Lease::agent`), in its non-interactive mode, with the prompt,
which also asks it to call a tool of the program. The example runs the tests
before and after, shows the diff, checks that the tools received the requests
of the agent with the token drawn for the sandbox (`Lease::mcp_token`), prints
the report of the launch and shuts the lease down.

### The MCP variants

The `-mcp` examples check the server against the MCP policy of the daemon,
register their own binary as a local MCP server (`sbx mcp add`), load it into
the gateway of the sandbox, call its `sum` tool from inside the sandbox with
the client of the [MCP examples](#mcp), then start the agent session: ask the
agent to call the `sum`, `echo` or `host_time` tool. When `sbx` starts the binary as the
server, it serves MCP on standard input and output instead
([`common/src/mcp.rs`](common/src/mcp.rs), built with
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)). The registration
is removed when the example ends.

## Unattended agents

The `-automation` examples run an agent the way a runner does: one prompt, no
terminal, a deadline, and a verification that does not trust the agent. Each
writes a small project whose test suite fails ([`common/task/`](common/task))
under `examples/target/automation/<run>/calc` on the host, and mounts it as the
workspace of a throw-away sandbox, with a pristine copy mounted read-only for
the diff. The agent runs in its non-interactive mode and changes the files of
the host, as with `sbx run`; the example then runs the tests again and prints
the diff and the hosts the agent reached. A prompt given on the command line
replaces the default task; the example fails when the tests still fail.

```sh
just example agent-claude-automation
just example agent-codex-automation "Add a div operation to calc.sh, with its tests"
```

| Agent | Example | Non-interactive command |
|---|---|---|
| Claude Code | [`agent-claude-automation`](agent-claude-automation/src/main.rs) | `claude --dangerously-skip-permissions -p <prompt> --output-format stream-json --verbose` |
| Codex | [`agent-codex-automation`](agent-codex-automation/src/main.rs) | `codex exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check --json <prompt>` |
| OpenCode | [`agent-opencode-automation`](agent-opencode-automation/src/main.rs) | `opencode run --auto <prompt>` |
| GitHub Copilot | [`agent-copilot-automation`](agent-copilot-automation/src/main.rs) | `copilot --yolo -p <prompt> --no-ask-user` |
| Cursor | [`agent-cursor-automation`](agent-cursor-automation/src/main.rs) | `cursor-agent --yolo -p <prompt> --trust --output-format stream-json` |
| Droid | [`agent-droid-automation`](agent-droid-automation/src/main.rs) | `droid exec --auto high -o stream-json <prompt>` |
| Devin | [`agent-devin-automation`](agent-devin-automation/src/main.rs) | `devin --permission-mode dangerous --respect-workspace-trust=false -p <prompt>` |
| Kiro | [`agent-kiro-automation`](agent-kiro-automation/src/main.rs) | `kiro chat --trust-all-tools --no-interactive <prompt>` |
| Docker Agent | [`agent-docker-agent-automation`](agent-docker-agent-automation/src/main.rs) | `docker-agent run --yolo --exec <prompt>` |

The program and its run options come from the kit itself
(`KitPackage::agent`); the non-interactive flags come from the documentation
of each agent, linked from each example. A credential is required, since
nobody can log in: a key of the table above, given through the proxy as for
the interactive examples, or the credential the daemon holds for the kit, a
Claude subscription for instance; without either, the agent fails. Kiro
has no service secret in `sbx`, so `KIRO_API_KEY` is passed to the agent
process as is and is visible inside the sandbox.

The network of the sandbox is not narrowed to the hosts of the agent: a
sandbox rule cannot override the global `default-allow-all` rule, since deny
rules take precedence over allow rules. The examples report instead what the
agent reached, from the activity log of the proxy.

## `common`

[`common`](common/src/lib.rs) holds what several examples share, so that each
example stays about the library: the connection, the kit cache (a kit alone,
composed with a mixin of the examples, or with kit arguments), throw-away
sandboxes (their creation reports the credentials the daemon withholds), the programs built for a sandbox and copied into it, the terminal
relay and what `sbx run` passes from the host, agent credentials, the MCP
servers (on standard input and output, or over streamable HTTP) and the client
run in the sandbox, the flow of the agent examples and that of the unattended
ones.
