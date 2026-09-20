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

    /// Short label for startup banners / TUI status lines.
    pub fn isolation_label(&self) -> &'static str {
        match self {
            SandboxRuntime::Bwrap(_) => "bwrap",
            SandboxRuntime::Legacy => "legacy",
        }
    }
}

/// Resolve the runtime for the configured mode (`auto` | `bwrap` | `legacy`).
/// Returns warnings so the caller can surface them to the user.
///
/// Detection never fails startup: without bwrap the file tools stay jailed
/// (PathJail), but `run_command` refuses to run unless the user opts in with
/// `legacy_commands = "allow"` — so a missing bwrap cannot silently turn into
/// unsandboxed command execution.
pub fn detect(configured: &str) -> (SandboxRuntime, Vec<String>) {
    let mut warnings = Vec::new();
    match configured {
        "legacy" => {
            warnings.push(legacy_warning("sandbox = \"legacy\" requested"));
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
        warnings.push(legacy_warning("bubblewrap (`bwrap`) not found"));
        return (SandboxRuntime::Legacy, warnings);
    };

    match self_test(&bwrap) {
        Ok(()) => (SandboxRuntime::Bwrap(bwrap), warnings),
        Err(e) => {
            warnings.push(legacy_warning(&format!(
                "bubblewrap self-test failed ({e}); on Ubuntu 24.04+ try enabling \
                 unprivileged user namespaces"
            )));
            (SandboxRuntime::Legacy, warnings)
        }
    }
}

/// Legacy-mode warning text. States the isolation loss plainly: file tools
/// are jailed, commands are not.
fn legacy_warning(cause: &str) -> String {
    format!(
        "{cause}: no filesystem isolation for commands; the model can read/write anywhere \
         the user can. run_command is refused unless you install bubblewrap or set \
         legacy_commands = \"allow\"; file tools remain jailed to the workspace."
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_detection_warning_states_the_isolation_loss() {
        let (runtime, warnings) = detect("legacy");
        assert_eq!(runtime, SandboxRuntime::Legacy);
        let text = warnings.join(" ");
        assert!(text.contains("no filesystem isolation"), "{text}");
        assert!(text.contains("read/write anywhere"), "{text}");
        assert!(text.contains("legacy_commands = \"allow\""), "{text}");
        assert!(text.contains("file tools remain jailed"), "{text}");
    }

    #[test]
    fn isolation_labels() {
        assert_eq!(SandboxRuntime::Legacy.isolation_label(), "legacy");
        assert_eq!(
            SandboxRuntime::Bwrap(PathBuf::from("/usr/bin/bwrap")).isolation_label(),
            "bwrap"
        );
    }
}
