//! Sandboxed workspace tools for lmhub-runner.
//!
//! - [`jail::PathJail`] confines every file operation to the model workspace;
//! - [`proc`] runs allowlisted commands with clean env, timeouts and
//!   process-group kills;
//! - [`tools::ToolRuntime`] implements the seven controlled tools.

pub mod jail;
pub mod proc;
pub mod tools;

pub use jail::PathJail;
pub use tools::{tool_specs, SandboxConfig, ToolOutcome, ToolRuntime};
