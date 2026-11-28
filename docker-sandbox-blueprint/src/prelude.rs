//! The types a program needs to describe a sandbox, for a single glob
//! import.
//!
//! Besides the types of this crate, it holds those of `docker-sandbox` and
//! `docker-sandbox-kit` a spec is written with: the client, the agents, the
//! MCP servers, the mixins and the mounts. It brings the `Error` and
//! `Result` of this crate, so it is not meant to be glob-imported beside the
//! prelude of `docker-sandbox`.
//!
//! ```
//! use docker_sandbox_blueprint::prelude::*;
//!
//! let spec = SandboxSpec::agent(Agent::Claude).with_mcp(McpServer::host("tools", 47813));
//! # let _ = spec;
//! ```

pub use crate::{Error, KeySpec, Keys, Result, SandboxSpec};
pub use docker_sandbox::Client;
pub use docker_sandbox::sandbox::{Secret, Workspace};
pub use docker_sandbox_kit::{Agent, KitDescriptor, LifecycleStep, McpServer, Mixin};
