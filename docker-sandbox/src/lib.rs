//! Typed async client for `sandboxd`, the daemon behind Docker Sandboxes
//! (`sbx`).
//!
//! `sandboxd` serves a private HTTP/1.1 API on a per-user Unix socket. The
//! `sbx` CLI is one client of that API; this crate is another, usable from any
//! tokio application without the CLI: create sandboxes from compiled kits,
//! keep them running, run commands and agent sessions, move files in and out,
//! publish ports, push credentials and scope network access.
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//! use docker_sandbox::prelude::*;
//!
//! # async fn run() -> docker_sandbox::Result<()> {
//! let client = Client::connect().await?;
//!
//! // Create a sandbox from a kit package compiled once with the CLI.
//! let package = KitPackage::open("/var/lib/kits/shell")?;
//! let created = package.create(&client, "job-42").await?;
//!
//! // Handles own a clone of the client: store them, move them into tasks.
//! let sandbox = client.sandbox(created.name);
//! let _running = sandbox.ensure_running().await?;
//! sandbox.allow_hosts(["api.github.com"]).await?;
//! sandbox.upload_dir("./repo", "/home/agent").await?;
//!
//! let output = sandbox
//!     .exec(
//!         &ExecSpec::shell("cd /home/agent/repo && make test")
//!             .with_timeout(Duration::from_secs(600)),
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
//! # Modules
//!
//! | Module | Entry point | Covers |
//! |---|---|---|
//! | [`sandbox`] | [`Client::sandbox`], [`Client::sandboxes`] | Listing, creation, life cycle, sessions, ports, files, credentials, SSH |
//! | [`exec`] | [`Sandbox::exec`](sandbox::Sandbox::exec), [`Sandbox::attach`](sandbox::Sandbox::attach) | Commands, interactive executions, agent sessions |
//! | [`policy`] | [`Client::policy`], [`Sandbox::allow_hosts`](sandbox::Sandbox::allow_hosts) | Network rules, checks, activity log |
//! | [`kit`] | [`KitPackage`](kit::KitPackage) | Compiled kit packages: creation, kits added in place or by recreation, compilation (feature `kit-compiler`) |
//! | [`image`] | [`Client::images`] | Images: list, pull, load, export, remove |
//! | [`event`] | [`Client::events`] | The daemon event stream |
//! | [`mcp`] | [`Client::mcp`], [`Sandbox::load_mcp_server`](sandbox::Sandbox::load_mcp_server) | MCP gateway, servers, policy checks |
//! | [`prompt`] | [`Client::prompts`] | Approval prompts raised by the sandboxes |
//! | [`daemon`] | [`Client::daemon`] | Health, settings, log levels, disk usage, diagnostics |
//!
//! [`prelude`] imports the types most programs need.
//!
//! # Things to know about the daemon
//!
//! - **No authentication.** Access is controlled by the socket file mode
//!   (`0600`): whoever can open the socket controls every sandbox of that
//!   user. Run as the user owning the daemon, never as root.
//! - **Private, moving API.** It is only versioned through `api_version`;
//!   [`Client::connect`] rejects versions outside [`SUPPORTED_API`] by default
//!   ([`VersionPolicy`]).
//! - **Sessions keep sandboxes alive.** A sandbox is stopped about 30 seconds
//!   after its last session closes; hold a
//!   [`SessionGuard`](sandbox::SessionGuard).
//! - **Some failures are reported in-band.** Sandbox creation and image pulls
//!   answer HTTP 200 and stream their progress; the crate reads the stream to
//!   its outcome and returns [`Error::Failed`].
//! - **Secrets travel in clear text** over the socket and are never readable
//!   back.
//!
//! # Errors
//!
//! Every operation returns [`Error`]. Daemon errors carry an [`ApiError`]
//! naming the failed operation; [`Error::is_not_found`],
//! [`Error::is_conflict`] and [`Error::is_transient`] classify them.
//!
//! # Streams
//!
//! Creation progress, image pulls, events, prompts and interactive output
//! implement [`futures_core::Stream`] and also expose an inherent `next()`
//! method, so that no stream library is required.
//!
//! # Scope
//!
//! Every route of the daemon is exposed except, deliberately:
//!
//! - `/daemon/reset` and `/daemon/shutdown`, which wipe or stop the daemon;
//! - `POST /policy/network/setup`, which changes the global network setup;
//! - `POST /oauth/{service}/reload`, which acts on the OAuth credentials the
//!   user stored with the CLI;
//! - `POST` and `DELETE /sandbox/{name}/debug/vm`, whose allow-lists of VM
//!   routes are unknown (`GET` is exposed as
//!   [`Sandbox::vm_metrics`](sandbox::Sandbox::vm_metrics)).
//!
//! # Features
//!
//! | Feature | Adds |
//! |---|---|
//! | `kit-compiler` | [`KitCompiler`](kit::KitCompiler) and [`CompileJob`](kit::CompileJob): compiling kit compositions with the `sbx` CLI |
//! | `live` | Nothing to the library: runs the integration tests against the real daemon |

#![cfg_attr(docsrs, feature(doc_cfg))]

mod client;
pub mod daemon;
mod error;
pub mod event;
pub mod exec;
pub mod image;
pub mod kit;
pub mod mcp;
pub mod policy;
pub mod prelude;
pub mod prompt;
pub mod sandbox;
mod transport;

pub use client::{
    ApiVersion, Client, ClientBuilder, InvalidApiVersion, SOCKET_ENV, SUPPORTED_API, VersionPolicy,
    VersionRange, default_socket_path,
};
pub use error::{ApiError, Error, Result};
pub use transport::NdjsonStream;

/// Fields of a daemon payload this crate does not model, kept so that a newer
/// daemon does not break decoding.
pub type Extra = serde_json::Map<String, serde_json::Value>;

/// Compiles the examples of the README as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
