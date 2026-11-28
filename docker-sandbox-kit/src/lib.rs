//! Kit packages of Docker Sandboxes (`sbx`), for the typed client of
//! [`docker-sandbox`](docker_sandbox).
//!
//! `sandboxd` cannot resolve kits by itself: `POST /sandbox` only takes kit
//! artifacts already compiled, and the images of local kit sources already
//! loaded. The `sbx` CLI does that work on the client side. This crate does
//! it for a program, and writes a
//! [`KitPackage`](docker_sandbox::kit::KitPackage), which `docker-sandbox`
//! replays through the socket:
//!
//! - [`KitForge`] (feature `forge`, on by default) forges it through the socket
//!   alone: the base kit rebuilt from its published image, with its
//!   arguments, and mixins described by a [`KitDescriptor`] or read from their
//!   directory. No `sbx` CLI, not even once.
//! - [`KitCompiler`] (feature `compiler`) compiles it once with the CLI,
//!   behind a proxy that captures what the CLI sends.
//!
//! It shares the version of `docker-sandbox`: use the two crates at the same
//! version. [`prelude`] imports its types, beside the prelude of
//! `docker-sandbox`.
//!
//! # Example
//!
//! The published claude kit, rebuilt from its image with two of its
//! arguments, and a mixin described in Rust: an install command and a network
//! rule. Everything is checked before the package is written.
//!
//! ```no_run
//! # #[cfg(feature = "forge")]
//! # async fn run() -> docker_sandbox_kit::Result<()> {
//! use docker_sandbox::prelude::*;
//! use docker_sandbox_kit::prelude::*;
//!
//! let client = Client::connect().await?;
//! let tools = KitDescriptor::mixin("Project tools")
//!     .with_install(LifecycleStep::new("pip install --user ruff").as_user("agent"))
//!     .with_network_deny(["*.pastebin.com"]);
//! let package = KitForge::new(client.clone())
//!     .with_base("docker.io/sbx/claude:latest")
//!     .with_arg("model", "claude-haiku-5-5")
//!     .with_mixin(Mixin::new("/kits/project-tools", tools)?)
//!     .forge("/var/lib/kits/claude+tools")
//!     .await?;
//!
//! // docker-sandbox replays the package through the socket.
//! let created = package.create(&client, "job-42").await?;
//! # let _ = created;
//! # Ok(())
//! # }
//! ```
//!
//! [`KitForge::build`] forges the same package in memory, without writing
//! anything: it creates sandboxes as it is, and
//! [`KitPackage::save`](docker_sandbox::kit::KitPackage::save) writes it when it
//! is to be kept. A package on disk is taken again by [`KitPackage::open`] for
//! the next sandboxes, while it suits the daemon
//! ([`KitPackage::suits`](docker_sandbox::kit::KitPackage::suits)), and
//! [`KitForge::with_base_package`] takes its base kit for another composition.
//! [`KitCache`] keeps the package of each composition, in memory and in a
//! directory when given one, and forges it again once it no longer suits.
//!
//! A kit named as the CLI names it is read with [`KitRef::base`] and
//! [`KitRef::mixin`], which follow the rules of `sbx create <kit>` and
//! `--kit <kit>`.
//!
//! [`Mixin::mcp`] gives the agent of a sandbox (claude, copilot or opencode)
//! its MCP servers ([`McpServer`]), served by a process of the host, remote, or
//! started in the sandbox, with nothing written on the host: its install
//! commands register them in the configuration of the agent, inside the
//! sandbox, and their tokens are given at creation, in variables the agent
//! reads when it connects.
//!
//! # Errors
//!
//! Every operation returns [`Error`]. What goes wrong in the client of
//! `docker-sandbox` (the socket, the daemon, a host file) keeps its
//! [`docker_sandbox::Error`], in [`Error::Client`].
//!
//! # Features
//!
//! | Feature | Adds |
//! |---|---|
//! | `forge` (default) | [`KitForge`], [`KitDescriptor`], [`LifecycleStep`], [`Mixin`], [`McpServer`] and [`KitCache`]: forging kit packages without the CLI, base kit rebuilt from its image, mixins and MCP servers for the agent, packages kept by composition |
//! | `compiler` | [`KitCompiler`] and [`CompileJob`]: compiling kit compositions with the `sbx` CLI |
//! | `live` | Nothing to the library: runs the integration tests against the real daemon |
//!
//! [`KitPackage::open`]: docker_sandbox::kit::KitPackage::open

#![cfg_attr(docsrs, feature(doc_cfg))]

mod agent;
#[cfg(feature = "compiler")]
mod compiler;
mod error;
#[cfg(feature = "forge")]
mod forge;
pub mod prelude;
mod reference;

pub use self::agent::{Agent, AgentKey, Headless, HeadlessOutput};
#[cfg(feature = "compiler")]
#[cfg_attr(docsrs, doc(cfg(feature = "compiler")))]
pub use self::compiler::{CompileJob, KitCompiler};
pub use self::error::{Error, Result};
#[cfg(feature = "forge")]
#[cfg_attr(docsrs, doc(cfg(feature = "forge")))]
pub use self::forge::{KitCache, KitDescriptor, KitForge, LifecycleStep, McpServer, Mixin};
pub use self::reference::KitRef;

/// Compiles the examples of the README as doctests, with the `forge`
/// feature its example forges the claude kit with.
#[cfg(all(doctest, feature = "forge"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
