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
//! It shares the version of `docker-sandbox` and `docker-sandbox-kit`: use
//! the three crates at the same version.
//!
//! [`prelude`] imports what a spec is written with, the types of the two
//! other crates included.
//!
//! # Example
//!
//! A claude sandbox on a project, its tools served on the host and its key
//! given at launch, checked against the daemon.
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
//! describes and checks one sandbox at a time.

mod error;
mod key;
mod mcp;
mod network;
pub mod prelude;
mod spec;

pub use self::error::{Error, Result};
pub use self::key::{KeySpec, Keys};
pub use self::spec::SandboxSpec;
