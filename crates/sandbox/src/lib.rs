//! Sandboxed workspace tools for lmhub-runner.
//!
//! - [`jail::PathJail`] confines every file operation to the model workspace;
//! - [`proc`] runs allowlisted commands with clean env, timeouts,
//!   process-group kills, resource limits and a seccomp deny-list;
//! - [`runtime::SandboxRuntime`] selects OS-level isolation (bubblewrap)
//!   with a loud legacy fallback;
//! - [`tools::ToolRuntime`] implements the seven controlled tools.

pub mod jail;
pub mod proc;
pub mod runtime;
pub mod tools;

pub use jail::PathJail;
pub use runtime::{detect as detect_runtime, SandboxRuntime};
pub use tools::{tool_specs, SandboxConfig, ToolOutcome, ToolRuntime};
