//! Allowlisted, sandboxed process execution for the `run_command` tool.
//!
//! - argv[0] must match the allowlist exactly (no shell interpretation);
//! - the binary is resolved by scanning `PATH` manually so a hostile
//!   `PATH` cannot smuggle anything in;
//! - the child runs with an *empty* environment (no runner secrets) plus a
//!   minimal bootstrap set (`PATH`, `HOME` inside the workspace, `TMPDIR`);
//! - cwd is the workspace jail root;
//! - every command has a timeout; on expiry the whole process group is
//!   killed (children included);
//! - stdout/stderr are captured to files inside the workspace `.tmp`.

use lmhub_core::{CoreError, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct CommandOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

impl CommandOutcome {
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// Resolve `program` against the allowlist and PATH. Returns the absolute
/// binary path.
fn resolve_binary(program: &str, allowed: &[String]) -> Result<PathBuf> {
    if program.is_empty() || program.contains(['/', '\\']) {
        return Err(CoreError::Sandbox(format!(
            "invalid program name {program:?}; pass a bare command like \"node\""
        )));
    }
    if !allowed.iter().any(|a| a == program) {
        return Err(CoreError::Sandbox(format!(
            "command {program:?} is not on the allowlist {:?}",
            allowed
        )));
    }
    let path_var = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(program);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                meta.is_file() && meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = meta.is_file();
            if executable {
                return candidate
                    .canonicalize()
                    .map_err(CoreError::Io)
                    .map_err(|e| CoreError::Sandbox(format!("cannot resolve binary: {e}")));
            }
        }
    }
    Err(CoreError::Sandbox(format!(
        "allowlisted command {program:?} not found on this machine"
    )))
}

/// Run one allowlisted command inside the workspace.
///
/// `out_id` must be unique per invocation; capture files land in `tmp_dir`.
/// When [`crate::runtime::SandboxRuntime::Bwrap`] is active the command runs
/// inside bubblewrap (user/pid/ipc/uts namespaces, read-only system dirs,
/// writable workspace only); otherwise it runs directly with rlimits +
/// seccomp as the isolation layer.
#[allow(clippy::too_many_arguments)]
pub async fn run_allowlisted(
    argv: &[String],
    allowed: &[String],
    jail_root: &Path,
    tmp_dir: &Path,
    home_dir: &Path,
    timeout: Duration,
    out_id: &str,
    runtime: &crate::runtime::SandboxRuntime,
) -> Result<CommandOutcome> {
    let Some(program) = argv.first() else {
        return Err(CoreError::Sandbox("argv must not be empty".into()));
    };
    let resolved = resolve_binary(program, allowed)?;

    std::fs::create_dir_all(tmp_dir)?;
    std::fs::create_dir_all(home_dir)?;
    let stdout_path = tmp_dir.join(format!("{out_id}.out"));
    let stderr_path = tmp_dir.join(format!("{out_id}.err"));
    let stdout_file = std::fs::File::create(&stdout_path)?;
    let stderr_file = std::fs::File::create(&stderr_path)?;

    const MINIMAL_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
    // nvm/homebrew/asdf installs live outside the minimal set; prepend the
    // resolved binary's directory so `npm` finds its own `node`.
    let bin_dir = resolved
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let child_path = if bin_dir.is_empty() {
        MINIMAL_PATH.to_string()
    } else {
        format!("{bin_dir}:{MINIMAL_PATH}")
    };

    use tokio::process::Command;
    let mut cmd = Command::new(&resolved);
    // Inside bubblewrap the jail root is the only writable location; the
    // resolved binary's prefix is bound read-only so nvm/homebrew installs
    // stay visible to `node`/`npm`/`npx`.
    if let crate::runtime::SandboxRuntime::Bwrap(bwrap) = runtime {
        cmd = Command::new(bwrap);
        let mut inner: Vec<String> = Vec::with_capacity(argv.len() + 24);
        inner.extend([
            "--die-with-parent".into(),
            "--unshare-user".into(),
            "--unshare-pid".into(),
            "--unshare-ipc".into(),
            "--unshare-uts".into(),
            "--ro-bind".into(),
            "/usr".into(),
            "/usr".into(),
            "--ro-bind".into(),
            "/bin".into(),
            "/bin".into(),
            "--ro-bind".into(),
            "/lib".into(),
            "/lib".into(),
            "--ro-bind".into(),
            "/lib64".into(),
            "/lib64".into(),
            "--ro-bind".into(),
            "/etc".into(),
            "/etc".into(),
            "--proc".into(),
            "/proc".into(),
            "--dev".into(),
            "/dev".into(),
            "--tmpfs".into(),
            "/tmp".into(),
            "--tmpfs".into(),
            "/run".into(),
        ]);
        // Bind the binary's prefix (parent of the bin dir) read-only, unless
        // it is already covered by a bound system dir.
        let prefix = resolved.parent().and_then(|p| p.parent());
        if let Some(prefix) = prefix {
            let prefix_str = prefix.to_string_lossy().to_string();
            if prefix != Path::new("/")
                && !["/usr", "/bin", "/lib", "/lib64"]
                    .iter()
                    .any(|d| prefix.starts_with(d))
            {
                inner.extend(["--ro-bind".into(), prefix_str.clone(), prefix_str]);
            }
        }
        inner.extend([
            "--bind".into(),
            jail_root.to_string_lossy().to_string(),
            jail_root.to_string_lossy().to_string(),
            "--chdir".into(),
            jail_root.to_string_lossy().to_string(),
            "--".into(),
            resolved.to_string_lossy().to_string(),
        ]);
        inner.extend(argv[1..].iter().cloned());
        cmd.args(&inner);
    } else {
        cmd.args(&argv[1..]);
    }
    cmd.current_dir(jail_root)
        .env_clear()
        .env("PATH", &child_path)
        .env("HOME", home_dir)
        .env("TMPDIR", tmp_dir)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .stdin(std::process::Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file)
        // If the agent loop is cancelled/timed out and this future is
        // dropped, do not leave an orphan running.
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        // New process group => we can kill the entire subtree on timeout.
        cmd.process_group(0);
        // Hard resource limits so a hostile command cannot OOM the host,
        // fill the disk, or fork-bomb before the wall-clock timeout fires.
        let cpu_secs = timeout.as_secs().saturating_add(5).max(60);
        let install_seccomp = !runtime.is_bwrap();
        unsafe {
            cmd.pre_exec(move || {
                set_rlimit(libc::RLIMIT_AS as u64, 2 * 1024 * 1024 * 1024);
                set_rlimit(libc::RLIMIT_CPU as u64, cpu_secs);
                set_rlimit(libc::RLIMIT_FSIZE as u64, 64 * 1024 * 1024);
                // High enough for npm's worker fan-out and CI parallelism,
                // low enough that a fork-bomb still dies fast.
                set_rlimit(libc::RLIMIT_NPROC as u64, 512);
                set_rlimit(libc::RLIMIT_NOFILE as u64, 1024);
                if install_seccomp {
                    // Only in legacy mode: bwrap itself needs unshare/clone-
                    // with-namespace-flags to create its sandbox, so the
                    // seccomp deny-list cannot apply to the wrapper. Under
                    // bwrap the same surface (ptrace, cross-process memory
                    // access, mounting host paths, namespace escapes) is
                    // already blocked by the pid/userns split + caps.
                    install_seccomp_filter();
                }
                Ok(())
            });
        }
    }

    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| CoreError::Sandbox(format!("failed to spawn {:?}: {e}", program)))?;

    let pid = child.id().unwrap_or(0);
    // While armed, dropping this guard SIGKILLs the whole process group
    // (covers loop cancellation and any other early return).
    let mut group_guard = ProcessGroupGuard::new(pid);

    enum WaitResult {
        TimedOut,
        Finished(std::process::ExitStatus),
        Failed(String),
    }

    let wait_result: WaitResult = {
        let wait = async {
            loop {
                match child.wait().await {
                    Ok(status) => break Ok(status),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => break Err(CoreError::Sandbox(format!("wait failed: {e}"))),
                }
            }
        };
        tokio::pin!(wait);
        match tokio::time::timeout(timeout, &mut wait).await {
            Err(_elapsed) => WaitResult::TimedOut,
            Ok(Ok(status)) => WaitResult::Finished(status),
            Ok(Err(e)) => WaitResult::Failed(e.to_string()),
        }
    };

    let timed_out = matches!(wait_result, WaitResult::TimedOut);
    if timed_out {
        #[cfg(unix)]
        {
            terminate_group(pid);
            let exited = tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .is_ok();
            if !exited {
                tracing::warn!(?pid, "run_command ignored SIGTERM; sending SIGKILL");
                kill_group(pid);
                // Reap after killing so no zombie remains.
                let _ = child.wait().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = child.kill().await;
        }
    }
    if let WaitResult::Failed(msg) = &wait_result {
        // Child may still be running; the guard's Drop kills the group.
        return Err(CoreError::Sandbox(msg.clone()));
    }
    // Child is reaped: disarming prevents signaling a recycled pid/group.
    group_guard.disarm();
    let exit_code = match &wait_result {
        WaitResult::Finished(status) => status.code(),
        _ => None,
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    let stdout_bytes = file_len(&stdout_path);
    let stderr_bytes = file_len(&stderr_path);

    Ok(CommandOutcome {
        exit_code,
        timed_out,
        duration_ms,
        stdout_path,
        stderr_path,
        stdout_bytes,
        stderr_bytes,
    })
}

/// While armed, `Drop` SIGKILLs the child's whole process group. This covers
/// loop cancellation and any early-return path without relying on tokio's
/// `kill_on_drop`, which only kills the direct child.
struct ProcessGroupGuard(Option<u32>);

impl ProcessGroupGuard {
    fn new(pid: u32) -> Self {
        Self(Some(pid))
    }

    /// Call only after the child has been reaped, so a recycled pid/group
    /// cannot be signaled by the guard's `Drop`.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            kill_group(pid);
        }
    }
}

/// SIGKILL the whole process group (negative pid targets the group created
/// via `process_group(0)`). Probes with `kill(pid, 0)` first so a group that
/// no longer exists is never signaled.
#[cfg(unix)]
fn kill_group(pid: u32) {
    let group = -(pid as libc::pid_t);
    if unsafe { libc::kill(group, 0) } == -1 {
        return;
    }
    unsafe {
        libc::kill(group, libc::SIGKILL);
    }
}

/// SIGTERM the whole process group (grace period before SIGKILL).
#[cfg(unix)]
fn terminate_group(pid: u32) {
    let group = -(pid as libc::pid_t);
    if unsafe { libc::kill(group, 0) } == -1 {
        return;
    }
    tracing::warn!(?pid, "run_command timed out; terminating process group");
    unsafe {
        libc::kill(group, libc::SIGTERM);
    }
}

#[cfg(unix)]
fn set_rlimit(resource: u64, value: u64) {
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    unsafe {
        // Linux-gnu libc models the resource argument as u32; other unixes
        // (musl, macOS, BSD) use c_int.
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        libc::setrlimit(resource as u32, &limit);
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        libc::setrlimit(resource as libc::c_int, &limit);
    }
}

/// Install a seccomp-BPF deny-list in the pre-exec child (legacy mode only —
/// under bwrap the namespace split provides the same protections):
/// - no new privileges (required for filter installation);
/// - `clone` with any namespace flag is denied (no nested namespaces);
/// - dangerous syscalls return EPERM: tracing, cross-process memory access,
///   mounts, kernel module/trace/cgroup manipulation, io_uring, bpf,
///   userfaultfd, perf, handle-based fd escapes, `unshare`/`setns`.
///
/// Default action is ALLOW, so a missed syscall never breaks node/npm/npx.
/// Only compiled for the release targets (linux x86_64 / aarch64); other
/// platforms get no filter (documented limitation).
#[cfg(target_os = "linux")]
fn install_seccomp_filter() {
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xC0_00_00_3E; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xC0_00_00_B7; // AUDIT_ARCH_AARCH64
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return; // unsupported arch — no filter (documented)

    #[cfg(target_arch = "x86_64")]
    const DENIED: &[u32] = &[
        101, 310, 311, 165, 166, 155, 246, 320, 175, 313, 176, 321, 323, 298, 304, 303, 425, 426,
        427, 272, 308, 56,
    ];
    #[cfg(target_arch = "aarch64")]
    const DENIED: &[u32] = &[
        117, 270, 271, 40, 39, 41, 104, 294, 105, 273, 106, 280, 282, 241, 304, 303, 425, 426, 427,
        97, 268, 220,
    ];
    #[cfg(target_arch = "x86_64")]
    const CLONE_NR: u32 = 56;
    #[cfg(target_arch = "aarch64")]
    const CLONE_NR: u32 = 220;
    // CLONE_NEWUSER|NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS|NEWCGROUP|NEWTIME
    const CLONE_NEW_MASK: u32 = 0x7E_02_00_80;

    // Classic BPF encoding for seccomp_data.
    const LD_ABS_ARCH: u16 = 0x20; // BPF_LD|BPF_W|BPF_ABS, offset 4
    const LD_ABS_NR: u16 = 0x20; // offset 0
    const LD_ABS_ARG0: u16 = 0x20; // offset 16
    const JEQ_K: u16 = 0x15; // BPF_JMP|BPF_JEQ|BPF_K
    const JA: u16 = 0x05; // BPF_JMP|BPF_JA
    const AND_K: u16 = 0x54; // BPF_ALU|BPF_AND|BPF_K
    const RET_K: u16 = 0x06; // BPF_RET|BPF_K
    const RET_ALLOW: u32 = 0x7FFF_0000;
    const RET_ERRNO_EPERM: u32 = 0x0005_0001;
    const RET_KILL: u32 = 0x8000_0000;

    let ins = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };

    // Jump targets are resolved as `i + 1 + jt` (JA: `i + 1 + k`) and must
    // land strictly inside the program — every jt below accounts for that.
    let mut f: Vec<libc::sock_filter> = Vec::with_capacity(8 + DENIED.len() * 3);
    f.push(ins(LD_ABS_ARCH, 0, 0, 4));
    f.push(ins(JEQ_K, 1, 0, ARCH)); // arch ok → skip the kill
    f.push(ins(RET_K, 0, 0, RET_KILL));
    f.push(ins(LD_ABS_NR, 0, 0, 0));
    for nr in DENIED {
        f.push(ins(JEQ_K, 1, 0, *nr)); // match → the RET right below
        f.push(ins(JA, 0, 0, 1)); // no match → next syscall's JEQ
        f.push(ins(RET_K, 0, 0, RET_ERRNO_EPERM));
    }
    // clone(nr == CLONE_NR): inspect flags in args[0].
    f.push(ins(JEQ_K, 1, 0, CLONE_NR));
    f.push(ins(JA, 0, 0, 4)); // not clone → skip the flags check
    f.push(ins(LD_ABS_ARG0, 0, 0, 16));
    f.push(ins(AND_K, 0, 0, CLONE_NEW_MASK));
    f.push(ins(JEQ_K, 1, 0, 0)); // clean flags → allow
    f.push(ins(RET_K, 0, 0, RET_ERRNO_EPERM));
    f.push(ins(RET_K, 0, 0, RET_ALLOW));

    let prog = libc::sock_fprog {
        len: f.len() as u16,
        filter: f.as_mut_ptr(),
    };
    unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        // libc does not expose seccomp(2) on all linux targets; call it via
        // the raw syscall (SYS_seccomp is defined per-arch).
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            &prog as *const _ as *mut libc::c_void,
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn install_seccomp_filter() {
    // No seccomp outside Linux (documented limitation).
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Read up to `max_bytes` from the tail of a capture file without loading
/// the whole file into memory (capture size is attacker-controlled).
pub fn read_capture_tail(path: &Path, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(meta) = file.metadata() else {
        return String::new();
    };
    let len = meta.len();
    let skip = len.saturating_sub(max_bytes);
    if skip > 0 && file.seek(SeekFrom::Start(skip)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::with_capacity(skip.min(max_bytes) as usize);
    if file.take(max_bytes).read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let mut text = String::from_utf8_lossy(&bytes).to_string();
    if skip > 0 {
        text.insert_str(0, "[...truncated...]\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::SandboxRuntime;

    /// Run every behavioral test under both backends when bwrap is usable.
    fn runtimes() -> Vec<SandboxRuntime> {
        let mut rt: Vec<SandboxRuntime> = vec![SandboxRuntime::Legacy];
        let (detected, _warnings) = crate::runtime::detect("auto");
        if detected.is_bwrap() {
            rt.push(detected);
        }
        rt
    }

    async fn run(
        runtime: &SandboxRuntime,
        argv: &[String],
        dir: &Path,
        id: &str,
        timeout: Duration,
    ) -> Result<CommandOutcome> {
        run_allowlisted(
            argv,
            &["node".to_string()],
            dir,
            &dir.join("t"),
            &dir.join("h"),
            timeout,
            id,
            runtime,
        )
        .await
    }

    #[tokio::test]
    async fn rejects_non_allowlisted_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run_allowlisted(
            &["curl".to_string(), "http://evil".to_string()],
            &["node".to_string()],
            tmp.path(),
            &tmp.path().join("t"),
            &tmp.path().join("h"),
            Duration::from_secs(5),
            "x1",
            &SandboxRuntime::Legacy,
        )
        .await;
        assert!(err.is_err());
        assert!(
            matches!(err.unwrap_err(), CoreError::Sandbox(_)),
            "must be a sandbox violation"
        );
    }

    #[tokio::test]
    async fn runs_allowlisted_command_with_clean_env() {
        for runtime in runtimes() {
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("LMHUB_PROC_TEST_SECRET", "abc123");
            let out = run(
                &runtime,
                &[
                    "node".to_string(),
                    "-e".to_string(),
                    "console.log(Object.keys(process.env).join(','))".to_string(),
                ],
                tmp.path(),
                "x2",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
            assert_eq!(out.exit_code, Some(0), "node should run under {runtime:?}");
            let envs = read_capture_tail(&out.stdout_path, 10_000);
            assert!(
                !envs.contains("LMHUB_PROC_TEST_SECRET"),
                "env must be clean: {envs}"
            );
            assert!(envs.contains("HOME") || envs.contains("PATH"), "{envs}");
        }
    }

    #[tokio::test]
    async fn times_out_and_kills() {
        for runtime in runtimes() {
            let tmp = tempfile::tempdir().unwrap();
            let out = run(
                &runtime,
                &[
                    "node".to_string(),
                    "-e".to_string(),
                    "setTimeout(()=>{},60000)".to_string(),
                ],
                tmp.path(),
                "x3",
                Duration::from_secs(2),
            )
            .await
            .unwrap();
            assert!(out.timed_out, "under {runtime:?}");
            assert!(out.duration_ms >= 1900 && out.duration_ms < 5000);
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn seccomp_filter_is_installed_in_legacy_mode() {
        // Under bwrap the deny-list cannot apply (bwrap needs the very
        // syscalls it would block) — namespaces provide the isolation there.
        let tmp = tempfile::tempdir().unwrap();
        let out = run(
            &SandboxRuntime::Legacy,
            &[
                "node".to_string(),
                "-e".to_string(),
                "process.stdout.write(require('fs').readFileSync('/proc/self/status','utf8'))"
                    .to_string(),
            ],
            tmp.path(),
            "x4",
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        let status = read_capture_tail(&out.stdout_path, 10_000);
        assert!(
            status.contains("Seccomp:\t2"),
            "seccomp mode 2 expected in legacy mode, got: {status}"
        );
    }

    #[tokio::test]
    async fn bwrap_blocks_writes_outside_the_workspace() {
        for runtime in runtimes() {
            if !runtime.is_bwrap() {
                continue; // legacy has no fs isolation
            }
            let tmp = tempfile::tempdir().unwrap();
            let out = run(
                &runtime,
                &[
                    "node".to_string(),
                    "-e".to_string(),
                    "require('fs').writeFileSync('/etc/lmhub-escape-test','x')".to_string(),
                ],
                tmp.path(),
                "x5",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
            assert_ne!(
                out.exit_code,
                Some(0),
                "writing to /etc must fail under bwrap"
            );
        }
    }

    #[tokio::test]
    async fn bwrap_hides_host_processes() {
        for runtime in runtimes() {
            if !runtime.is_bwrap() {
                continue;
            }
            let tmp = tempfile::tempdir().unwrap();
            let out = run(
                &runtime,
                &[
                    "node".to_string(),
                    "-e".to_string(),
                    "process.stdout.write(require('fs').readdirSync('/proc').filter(x=>/^\\d+$/.test(x)).length.toString())"
                        .to_string(),
                ],
                tmp.path(),
                "x6",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
            let count: usize = read_capture_tail(&out.stdout_path, 100)
                .trim()
                .parse()
                .unwrap_or(usize::MAX);
            assert!(
                count < 50,
                "sandboxed /proc must not expose the host process table (saw {count})"
            );
        }
    }
}
