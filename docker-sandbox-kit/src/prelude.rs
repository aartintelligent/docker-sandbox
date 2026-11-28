//! The types of this crate most programs need, for a single glob import.
//!
//! It holds the types of this crate alone, each behind its feature, and
//! neither its `Error` nor its `Result`: it is meant to be glob-imported
//! beside the prelude of `docker-sandbox`, which brings the client, its
//! `Error` and its `Result`.
//!
//! ```
//! use docker_sandbox::prelude::*;
//! use docker_sandbox_kit::prelude::*;
//!
//! # #[cfg(feature = "forge")]
//! let tools = KitDescriptor::mixin("Acme tools")
//!     .with_install(LifecycleStep::new("pip install acme-cli").as_user("agent"));
//! # #[cfg(feature = "forge")]
//! # let _ = tools;
//! assert_eq!(Agent::Claude.reference(), "docker.io/sbx/claude:latest");
//! ```

pub use crate::{Agent, AgentKey, Headless, HeadlessOutput, KitRef};
#[cfg(feature = "compiler")]
#[cfg_attr(docsrs, doc(cfg(feature = "compiler")))]
pub use crate::{CompileJob, KitCompiler};
#[cfg(feature = "forge")]
#[cfg_attr(docsrs, doc(cfg(feature = "forge")))]
pub use crate::{KitCache, KitDescriptor, KitForge, LifecycleStep, McpServer, Mixin};
