//! A whole sandbox of Docker Sandboxes (`sbx`) described as data, for the
//! typed client of [`docker-sandbox`](docker_sandbox).
//!
//! A [`SandboxSpec`] describes everything a sandbox receives but its name and
//! the values of its keys: its kit (an agent or a published base kit, its
//! arguments, mixins), its workspace and mounts, its network rules, the API
//! keys the proxy writes for it ([`KeySpec`]), the MCP servers of its agent,
//! its variables and its resources. It is written in Rust with chained
//! methods, from [`SandboxSpec::agent`] or [`SandboxSpec::base`]. One spec
//! serves every sandbox of a job, and holds nothing secret: the values of the
//! keys come at launch ([`Keys`]).
//!
//! Each field becomes the call of a brick of `docker-sandbox` or of
//! [`docker-sandbox-kit`](docker_sandbox_kit): the kit is forged without the
//! `sbx` CLI, its network rules are those of a mixin, the keys are
//! `ProxiedKey`s. The crate adds no call to the daemon of its own, and keeps
//! no state outside it. [`SandboxSpec::check`] checks a spec against the
//! rules of a launch and the kit policy of the daemon, without pulling or
//! writing anything.
//!
//! [`SandboxSpec::launch`] applies it: the kit forged in memory, a token
//! drawn for each MCP server of the host, the keys given to the proxy, the
//! sandbox created and started. It returns a [`Lease`], which owns the
//! sandbox and its session: the program works in it with the whole API of
//! `docker-sandbox`, then [`Lease::shutdown`] removes it, its network rules
//! and its keys with it. A launch that fails once the sandbox exists removes
//! it, unless the spec keeps it ([`Error::Launch`]). The [`Report`] of a
//! launch says what it did, and never holds the value of a key or a token.
//!
//! It shares the version of `docker-sandbox` and `docker-sandbox-kit`: use
//! the three crates at the same version.
//!
//! [`prelude`] imports what a spec is written with and its lease used with,
//! the types of the two other crates included.
//!
//! # Example
//!
//! A claude sandbox on a project, its tools served on the host and its key
//! given at launch, checked against the daemon, launched, then removed.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use docker_sandbox_blueprint::prelude::*;
//!
//! let spec = SandboxSpec::agent(Agent::Claude)
//!     .with_arg("model", "claude-haiku-5-5")
//!     .with_workspace("/srv/project")
//!     .with_network_deny(["*.pastebin.com"])
//!     .with_key(KeySpec::for_service("ANTHROPIC_API_KEY", "anthropic"))
//!     .with_mcp(McpServer::host("tools", 47813));
//! let client = Client::connect().await?;
//! spec.check(&client).await?;
//! let keys = Keys::new().with_key("ANTHROPIC_API_KEY", std::env::var("ANTHROPIC_API_KEY")?);
//! spec.check_keys(&keys)?;
//! let lease = spec.launch(&client, "job-42", &keys).await?;
//! // The server of the host checks this token in the requests of the agent.
//! let token = lease.mcp_token("tools");
//! # let _ = token;
//! lease.sandbox().exec(&ExecSpec::shell("git status")).await?;
//! println!("{}", serde_json::to_string(lease.report())?);
//! lease.shutdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # From another language
//!
//! A spec also has a JSON form (serde), for a program in another language
//! that hands its sandboxes to a Rust one: the same fields, an unknown one
//! refused. A Rust program never needs it.
//!
//! ```
//! # use docker_sandbox_blueprint::prelude::*;
//! let spec = SandboxSpec::agent(Agent::Claude).with_network_deny(["*.pastebin.com"]);
//! let json = serde_json::json!({ "agent": "claude", "network": { "deny": ["*.pastebin.com"] } });
//! assert_eq!(serde_json::to_value(&spec)?, json);
//! assert_eq!(serde_json::from_value::<SandboxSpec>(json)?, spec);
//! # Ok::<(), serde_json::Error>(())
//! ```
//!
//! # Out of its scope
//!
//! Runners, queues, scheduling, retries, pools of sandboxes and any HTTP API
//! or command line on top: those live in another repository. The crate
//! describes, checks and launches one sandbox at a time, and keeps nothing
//! outside the daemon.

mod error;
mod key;
mod launch;
mod mcp;
mod network;
pub mod prelude;
mod spec;

pub use self::error::{Error, LaunchStep, Result};
pub use self::key::{KeyReport, KeySpec, Keys, LeftOutKey, LeftOutReason};
pub use self::launch::{Lease, Report};
pub use self::mcp::ServerReport;
pub use self::network::NetworkReport;
pub use self::spec::SandboxSpec;

/// Compiles the examples of the README as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
