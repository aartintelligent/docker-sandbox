//! How each agent takes its keys and runs a prompt without a terminal.
//!
//! Neither the daemon nor the kit of an agent says which variables the
//! agent reads its API key from, nor how it runs one prompt and exits: that
//! is in the documentation of each agent, and this module writes it down
//! once, for every program that runs agents unattended. The program and the
//! options its sessions start it with come from its kit (`KitAgent`); the
//! rest of each command comes from the page linked on its agent. The
//! examples of the repository run each of them on a task.

use docker_sandbox::kit::KitAgent;

use super::Agent;

/// An API key an agent takes: the service the kits declare it for, and the
/// variables the agent reads it from, the first one preferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AgentKey {
    /// Service of the key, as the kits and `sbx secret set` name it, e.g.
    /// `anthropic`.
    pub service: &'static str,
    /// Variables the agent reads the key from, in order of preference.
    pub env: &'static [&'static str],
}

/// How an agent runs one prompt without a terminal, and exits.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Headless {
    /// The command, the prompt included, to run in the sandbox.
    pub command: Vec<String>,
    /// What the command writes on its standard output.
    pub output: HeadlessOutput,
}

/// What an agent run without a terminal writes on its standard output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HeadlessOutput {
    /// Text, for a person.
    Text,
    /// One JSON event per line: the steps, the tools called, the result.
    JsonLines,
}

/// The keys of the model providers the agents that take several of them
/// read.
const PROVIDERS: &[AgentKey] = &[
    AgentKey {
        service: "anthropic",
        env: &["ANTHROPIC_API_KEY"],
    },
    AgentKey {
        service: "openai",
        env: &["OPENAI_API_KEY"],
    },
    AgentKey {
        service: "google",
        env: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    },
    AgentKey {
        service: "openrouter",
        env: &["OPENROUTER_API_KEY"],
    },
    AgentKey {
        service: "groq",
        env: &["GROQ_API_KEY"],
    },
    AgentKey {
        service: "mistral",
        env: &["MISTRAL_API_KEY"],
    },
    AgentKey {
        service: "nebius",
        env: &["NEBIUS_API_KEY"],
    },
    AgentKey {
        service: "xai",
        env: &["XAI_API_KEY"],
    },
];

/// The key of Claude Code.
const CLAUDE: &[AgentKey] = &[AgentKey {
    service: "anthropic",
    env: &["ANTHROPIC_API_KEY"],
}];

/// The key of Codex.
const CODEX: &[AgentKey] = &[AgentKey {
    service: "openai",
    env: &["OPENAI_API_KEY", "CODEX_API_KEY"],
}];

/// The token of GitHub Copilot.
const COPILOT: &[AgentKey] = &[AgentKey {
    service: "copilot",
    env: &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"],
}];

/// The key of Cursor.
const CURSOR: &[AgentKey] = &[AgentKey {
    service: "cursor",
    env: &["CURSOR_API_KEY"],
}];

/// The key of Devin.
const DEVIN: &[AgentKey] = &[AgentKey {
    service: "devin",
    env: &["DEVIN_API_KEY"],
}];

/// The key of Droid.
const DROID: &[AgentKey] = &[AgentKey {
    service: "droid",
    env: &["FACTORY_API_KEY"],
}];

impl Agent {
    /// The API keys the agent takes, by the service of the kits: give each
    /// to the proxy as a `ProxiedKey` of its service, its value read from the
    /// first of its variables the program has. Empty for `shell`, and for
    /// `kiro`, which `sbx` knows no service for ([`Agent::forwarded_env`]).
    ///
    /// ```
    /// use docker_sandbox_kit::Agent;
    ///
    /// let key = &Agent::Claude.keys()[0];
    /// assert_eq!((key.service, key.env), ("anthropic", &["ANTHROPIC_API_KEY"][..]));
    /// assert_eq!(Agent::OpenCode.keys().len(), 8);
    /// ```
    #[must_use]
    pub fn keys(self) -> &'static [AgentKey] {
        match self {
            Self::Claude => CLAUDE,
            Self::Codex => CODEX,
            Self::Copilot => COPILOT,
            Self::Cursor => CURSOR,
            Self::Devin => DEVIN,
            Self::DockerAgent | Self::OpenCode => PROVIDERS,
            Self::Droid => DROID,
            Self::Kiro | Self::Shell => &[],
        }
    }

    /// The variables the agent reads a key from that `sbx` knows no service
    /// for: they go into the sandbox as they are, so their values are
    /// visible inside it. `KIRO_API_KEY` for `kiro`, none for the others.
    #[must_use]
    pub fn forwarded_env(self) -> &'static [&'static str] {
        match self {
            Self::Kiro => &["KIRO_API_KEY"],
            _ => &[],
        }
    }

    /// How the agent runs `prompt` without a terminal and exits, its program
    /// and the options of its sessions taken from `agent`, the agent its kit
    /// declares (`KitPackage::agent`, `Lease::agent` of
    /// `docker-sandbox-blueprint`). `None` for `shell`, and for a kit that
    /// declares no program for an agent whose command starts with the
    /// options of its sessions.
    ///
    /// | Agent | Command | Output | Documentation |
    /// |---|---|---|---|
    /// | `claude` | program, options, `-p <prompt> --output-format stream-json --verbose` | JSON lines | <https://code.claude.com/docs/en/headless> |
    /// | `codex` | program, `exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check --json <prompt>` | JSON lines | <https://learn.chatgpt.com/docs/non-interactive-mode> |
    /// | `copilot` | program, options, `-p <prompt> --no-ask-user` | text | <https://docs.github.com/en/copilot/how-tos/copilot-cli/automate-copilot-cli/run-cli-programmatically> |
    /// | `cursor` | program, options, `-p <prompt> --trust --output-format stream-json` | JSON lines | <https://cursor.com/docs/cli/headless> |
    /// | `devin` | program, options, `-p <prompt>` | text | <https://docs.devin.ai/cli/reference/commands> |
    /// | `docker-agent` | program, options, `--exec <prompt>` | text | <https://docs.docker.com/ai/docker-agent/guides/headless/> |
    /// | `droid` | program, `exec --auto high -o stream-json <prompt>` | JSON lines | <https://docs.factory.ai/cli/droid-exec/overview> |
    /// | `kiro` | program, options, `--no-interactive <prompt>` | text | <https://kiro.dev/docs/cli/headless> |
    /// | `opencode` | program, `run --auto <prompt>` | text | <https://opencode.ai/docs/cli/> |
    ///
    /// The options of the sessions of a kit let the agent act without
    /// asking (`--dangerously-skip-permissions` for claude, `--yolo` for
    /// copilot and cursor, …); an agent whose command starts with a
    /// subcommand takes the options the subcommand needs instead.
    ///
    /// ```no_run
    /// # fn run(package: docker_sandbox::kit::KitPackage) -> Option<()> {
    /// use docker_sandbox_kit::{Agent, HeadlessOutput};
    ///
    /// let headless = Agent::Claude.headless(&package.agent()?, "Fix the tests")?;
    /// assert_eq!(headless.output, HeadlessOutput::JsonLines);
    /// # Some(())
    /// # }
    /// ```
    #[must_use]
    pub fn headless(self, agent: &KitAgent, prompt: &str) -> Option<Headless> {
        let program = |fallback: &str| agent.binary.clone().unwrap_or_else(|| fallback.to_owned());
        let with_options = |args: &[&str]| {
            let command = agent.command(args.iter().copied());
            (!command.is_empty()).then_some(command)
        };
        let after = |program: String, args: &[&str]| {
            let mut command = vec![program];
            command.extend(args.iter().map(|&arg| arg.to_owned()));
            command.push(prompt.to_owned());
            Some(command)
        };
        let (command, output) = match self {
            Self::Claude => (
                with_options(&["-p", prompt, "--output-format", "stream-json", "--verbose"]),
                HeadlessOutput::JsonLines,
            ),
            Self::Codex => (
                after(
                    program("codex"),
                    &[
                        "exec",
                        "--dangerously-bypass-approvals-and-sandbox",
                        "--skip-git-repo-check",
                        "--json",
                    ],
                ),
                HeadlessOutput::JsonLines,
            ),
            Self::Copilot => (
                with_options(&["-p", prompt, "--no-ask-user"]),
                HeadlessOutput::Text,
            ),
            Self::Cursor => (
                with_options(&["-p", prompt, "--trust", "--output-format", "stream-json"]),
                HeadlessOutput::JsonLines,
            ),
            Self::Devin => (with_options(&["-p", prompt]), HeadlessOutput::Text),
            Self::DockerAgent => (with_options(&["--exec", prompt]), HeadlessOutput::Text),
            Self::Droid => (
                after(
                    program("droid"),
                    &["exec", "--auto", "high", "-o", "stream-json"],
                ),
                HeadlessOutput::JsonLines,
            ),
            Self::Kiro => (
                with_options(&["--no-interactive", prompt]),
                HeadlessOutput::Text,
            ),
            Self::OpenCode => (
                after(program("opencode"), &["run", "--auto"]),
                HeadlessOutput::Text,
            ),
            Self::Shell => (None, HeadlessOutput::Text),
        };
        command.map(|command| Headless { command, output })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The agent its kit declares for `name`, with a program and options.
    fn kit_agent(name: &str, binary: Option<&str>, options: &[&str]) -> KitAgent {
        KitAgent::from_artifact(&serde_json::json!({
            "manifest": { "name": name, "binary": binary, "runOptions": options }
        }))
        .unwrap()
    }

    /// Checks that `agent`, its kit declaring `binary` and `options`, runs
    /// `Fix the tests` with `command`, written with spaces, and `output`.
    fn runs(
        agent: Agent,
        binary: Option<&str>,
        options: &[&str],
        command: &str,
        output: HeadlessOutput,
    ) {
        let headless = agent
            .headless(&kit_agent(agent.name(), binary, options), "Fix the tests")
            .unwrap();
        let expected: Vec<String> = command
            .replace("<prompt>", "Fix\u{0}the\u{0}tests")
            .split(' ')
            .map(|word| word.replace('\u{0}', " "))
            .collect();
        assert_eq!(headless.command, expected, "{agent}");
        assert_eq!(headless.output, output, "{agent}");
    }

    #[test]
    fn every_agent_runs_a_prompt_as_its_documentation_says() {
        use HeadlessOutput::{JsonLines, Text};
        let skip = &["--dangerously-skip-permissions"][..];
        runs(
            Agent::Claude,
            Some("claude"),
            skip,
            "claude --dangerously-skip-permissions -p <prompt> --output-format stream-json --verbose",
            JsonLines,
        );
        runs(
            Agent::Codex,
            None,
            &["--yolo"],
            "codex exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check --json <prompt>",
            JsonLines,
        );
        runs(
            Agent::Copilot,
            Some("copilot"),
            &["--yolo"],
            "copilot --yolo -p <prompt> --no-ask-user",
            Text,
        );
        runs(
            Agent::Cursor,
            Some("cursor-agent"),
            &["--yolo"],
            "cursor-agent --yolo -p <prompt> --trust --output-format stream-json",
            JsonLines,
        );
        runs(
            Agent::Devin,
            Some("devin"),
            &["--permission-mode", "dangerous"],
            "devin --permission-mode dangerous -p <prompt>",
            Text,
        );
        runs(
            Agent::DockerAgent,
            Some("docker-agent"),
            &["run", "--yolo"],
            "docker-agent run --yolo --exec <prompt>",
            Text,
        );
        runs(
            Agent::Droid,
            Some("droid"),
            &[],
            "droid exec --auto high -o stream-json <prompt>",
            JsonLines,
        );
        runs(
            Agent::Kiro,
            Some("kiro-cli"),
            &["chat", "--trust-all-tools"],
            "kiro-cli chat --trust-all-tools --no-interactive <prompt>",
            Text,
        );
        runs(
            Agent::OpenCode,
            Some("opencode"),
            &[],
            "opencode run --auto <prompt>",
            Text,
        );

        let shell = kit_agent("shell", None, &[]);
        assert_eq!(Agent::Shell.headless(&shell, "Fix the tests"), None);
        // An agent whose command starts with the options of its sessions
        // needs the program its kit declares.
        assert_eq!(Agent::Claude.headless(&shell, "Fix the tests"), None);
    }

    #[test]
    fn every_agent_names_its_keys() {
        for agent in Agent::ALL {
            let keys = agent.keys();
            assert!(keys.iter().all(|key| !key.env.is_empty()), "{agent}");
            match agent {
                Agent::Shell => assert!(keys.is_empty() && agent.forwarded_env().is_empty()),
                Agent::Kiro => assert_eq!(
                    (keys.len(), agent.forwarded_env()),
                    (0, &["KIRO_API_KEY"][..])
                ),
                _ => assert!(
                    !keys.is_empty() && agent.forwarded_env().is_empty(),
                    "{agent}"
                ),
            }
        }
        assert_eq!(
            Agent::Codex.keys()[0].env,
            ["OPENAI_API_KEY", "CODEX_API_KEY"]
        );
        assert_eq!(Agent::DockerAgent.keys(), Agent::OpenCode.keys());
    }
}
