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
//! Its version follows the kit compiler of the CLI, whose output the crate
//! writes, rather than the API of the daemon, which `docker-sandbox` follows.
//!
//! # Example
//!
//! ```no_run
//! # #[cfg(feature = "forge")]
//! # async fn run() -> docker_sandbox_kit::Result<()> {
//! use std::time::Duration;
//!
//! use docker_sandbox::prelude::*;
//! use docker_sandbox_kit::KitForge;
//!
//! let client = Client::connect().await?;
//!
//! // The claude kit, rebuilt from its published image through the socket.
//! // The daemon treats the sandbox as one the CLI created, and gives the
//! // agent the credential it holds for the kit, a Claude subscription for
//! // instance.
//! let package = KitForge::new(client.clone())
//!     .with_base("docker.io/sbx/claude:latest")
//!     .with_arg("model", "claude-haiku-5-5")
//!     .forge("/var/lib/kits/claude")
//!     .await?;
//! let created = package.create(&client, "job-42").await?;
//!
//! let sandbox = client.sandbox(created.name);
//! let _running = sandbox.ensure_running().await?;
//! sandbox.upload_dir("./repo", "/home/agent").await?;
//! let output = sandbox
//!     .exec(
//!         &ExecSpec::new([
//!             "claude",
//!             "--dangerously-skip-permissions",
//!             "-p",
//!             "Run the tests, fix what fails, and summarize the fix.",
//!         ])
//!         .with_working_dir("/home/agent/repo")
//!         .with_timeout(Duration::from_secs(900)),
//!     )
//!     .await?
//!     .check()?;
//! println!("{}", output.stdout);
//!
//! sandbox.force_delete().await?;
//! # Ok(())
//! # }
//! ```
//!
//! The package stays on disk: [`KitPackage::open`] takes it again for the
//! next sandboxes, without forging it again.
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
//! | `forge` (default) | [`KitForge`], [`KitDescriptor`], [`LifecycleStep`] and [`Mixin`]: forging kit packages without the CLI, base kit rebuilt from its image and mixins |
//! | `compiler` | [`KitCompiler`] and [`CompileJob`]: compiling kit compositions with the `sbx` CLI |
//! | `live` | Nothing to the library: runs the integration tests against the real daemon |
//!
//! [`KitPackage::open`]: docker_sandbox::kit::KitPackage::open

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "compiler")]
mod compiler;
mod error;
#[cfg(feature = "forge")]
mod forge;

#[cfg(feature = "compiler")]
#[cfg_attr(docsrs, doc(cfg(feature = "compiler")))]
pub use self::compiler::{CompileJob, KitCompiler};
pub use self::error::{Error, Result};
#[cfg(feature = "forge")]
#[cfg_attr(docsrs, doc(cfg(feature = "forge")))]
pub use self::forge::{KitDescriptor, KitForge, LifecycleStep, Mixin};

/// Compiles the examples of the README as doctests, with the `forge`
/// feature its example forges the claude kit with.
#[cfg(all(doctest, feature = "forge"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
