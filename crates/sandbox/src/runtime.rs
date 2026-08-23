//! OS-level isolation backend for `run_command`.
//!
//! - [`SandboxRuntime::Bwrap`]: bubblewrap provides user/pid/ipc/uts
//!   namespaces, a read-only system view and a writable workspace bind.
//!   Network stays allowed (npm install keeps working).
//! - [`SandboxRuntime::Legacy`]: rlimits + process-group kill only — the
//!   same behavior as before isolation was introduced. Used when bwrap is
//!   missing or user namespaces are blocked (never silently).

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxRuntime {
    /// Bubblewrap binary path (already capability-tested).
    Bwrap(PathBuf),
    /// No OS isolation (warns loudly at detection time).
    Legacy,
}

impl SandboxRuntime {
    pub fn is_bwrap(&self) -> bool {
        matches!(self, SandboxRuntime::Bwrap(_))
    }
}

/// Resolve the runtime for the configured mode (`auto` | `bwrap` | `legacy`).
/// Returns warnings so the caller can surface them to the user.
pub fn detect(configured: &str) -> (SandboxRuntime, Vec<String>) {
    let mut warnings = Vec::new();
    match configured {
        "legacy" => {
            warnings.push("sandbox = \"legacy\": run_command has NO OS-level isolation".into());
            return (SandboxRuntime::Legacy, warnings);
        }
        "bwrap" | "auto" => {}
        other => {
            warnings.push(format!(
                "unknown sandbox mode {other:?}; using auto-detection"
            ));
        }
    }

    let bwrap = find_bwrap();
    let Some(bwrap) = bwrap else {
        warnings.push(
            "bubblewrap (`bwrap`) not found — run_command runs WITHOUT OS-level isolation; \
             install bubblewrap for the sandboxed backend"
                .into(),
        );
        return (SandboxRuntime::Legacy, warnings);
    };

    match self_test(&bwrap) {
        Ok(()) => (SandboxRuntime::Bwrap(bwrap), warnings),
        Err(e) => {
            warnings.push(format!(
                "bubblewrap self-test failed ({e}) — run_command runs WITHOUT OS-level \
                 isolation; on Ubuntu 24.04+ try enabling unprivileged user namespaces"
            ));
            (SandboxRuntime::Legacy, warnings)
        }
    }
}

fn find_bwrap() -> Option<PathBuf> {
    if let Ok(configured) = std::env::var("LMHUB_BWRAP") {
        if !configured.trim().is_empty() {
            let p = PathBuf::from(configured.trim());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let path_var = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join("bwrap");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Prove the local bwrap can actually isolate (namespaces available).
fn self_test(bwrap: &Path) -> std::result::Result<(), String> {
    // /usr/bin/true: on symlinked distros (e.g. /bin -> /usr/bin) the bind
    // of /bin does not carry a working /bin/true into the sandbox.
    let cmd_path = if Path::new("/usr/bin/true").exists() {
        "/usr/bin/true"
    } else {
        "/bin/true"
    };
    let mut cmd = Command::new(bwrap);
    cmd.args([
        "--die-with-parent",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--ro-bind",
        "/usr",
        "/usr",
        "--ro-bind",
        "/bin",
        "/bin",
        "--ro-bind",
        "/lib",
        "/lib",
        "--ro-bind",
        "/lib64",
        "/lib64",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--",
        cmd_path,
    ])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("bwrap exited {s}")),
        Err(e) => Err(e.to_string()),
    }
}
