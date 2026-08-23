//! The seven controlled tools handed to the model. Every file operation is
//! jailed to the workspace; every command runs allowlisted and time-boxed.

use crate::jail::PathJail;
use crate::proc as sandbox_proc;
use lmhub_core::{CoreError, Result, ToolSpec};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

pub const TOOL_NAMES: [&str; 7] = [
    "list_directory",
    "read_file",
    "write_file",
    "edit_file",
    "create_directory",
    "run_command",
    "read_command_output",
];

/// Static tool definitions (provider-neutral; adapters map to wire formats).
pub fn tool_specs() -> Vec<ToolSpec> {
    fn obj(schema: Value) -> Value {
        json!({"type": "object", "properties": schema})
    }
    vec![
        ToolSpec {
            name: "list_directory".into(),
            description: "List entries of a directory inside your workspace. Paths are \
                          workspace-relative; absolute paths are re-based onto the workspace.\
                          \nExample: {\"path\": \"src\"}"
                .into(),
            parameters: obj(json!({
                "path": {"type": "string", "description": "Directory path, relative to workspace root. Use \".\" or omit for root."}
            })),
        },
        ToolSpec {
            name: "read_file".into(),
            description: "Read a text file from your workspace. Output is truncated beyond a \
                          safe limit; use offset_line to page through long files.\
                          \nExample: {\"path\": \"package.json\"}"
                .into(),
            parameters: obj(json!({
                "path": {"type": "string", "description": "File path relative to workspace."},
                "offset_line": {"type": "integer", "description": "0-based first line to return (default 0)."},
                "max_bytes": {"type": "integer", "description": "Optional byte cap (server-enforced hard cap applies)."}
            })),
        },
        ToolSpec {
            name: "write_file".into(),
            description: "Create or overwrite a text file in your workspace. Parent directories \
                          are created automatically. Content must be full final content.\
                          \nExample: {\"path\": \"index.js\", \"content\": \"console.log('hi')\\n\"}"
                .into(),
            parameters: obj(json!({
                "path": {"type": "string", "description": "File path relative to workspace."},
                "content": {"type": "string", "description": "Full file content."}
            })),
        },
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace an exact substring in an existing text file. old_string must \
                          match exactly and uniquely unless replace_all is true. Read the file \
                          first to learn its exact content."
                .into(),
            parameters: obj(json!({
                "path": {"type": "string", "description": "File path relative to workspace."},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
            })),
        },
        ToolSpec {
            name: "create_directory".into(),
            description: "Create a directory (and parents) inside your workspace.".into(),
            parameters: obj(json!({
                "path": {"type": "string"}
            })),
        },
        ToolSpec {
            name: "run_command".into(),
            description: "Run an ALLOWLISTED command inside the workspace. Only specific \
                          commands are available (e.g. node/npm/npx); arbitrary binaries and \
                          shells are rejected. Pass argv as an array of strings — no shell \
                          syntax, no pipes/redirects. Output is captured; retrieve it with \
                          read_command_output.\
                          \nExample: {\"argv\": [\"node\", \"--version\"]}"
                .into(),
            parameters: obj(json!({
                "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                         "description": "Command and arguments, e.g. [\"npm\", \"init\", \"-y\"]"},
                "timeout_secs": {"type": "integer", "description": "Optional timeout 5..=300 (server default applies)."}
            })),
        },
        ToolSpec {
            name: "read_command_output".into(),
            description: "Read captured stdout/stderr of the most recent run_command call."
                .into(),
            parameters: obj(json!({})),
        },
    ]
}

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub allowed_commands: Vec<String>,
    pub command_timeout: Duration,
    pub read_file_max_bytes: u64,
    pub write_file_max_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub success: bool,
    /// Bounded human-readable result returned to the model.
    pub summary: String,
    /// Safe metadata persisted to events.jsonl.
    pub metadata: Value,
    pub error: Option<String>,
    pub sandbox_violation: bool,
    pub duration_ms: u64,
}

impl ToolOutcome {
    fn ok(summary: impl Into<String>, metadata: Value) -> Self {
        Self {
            success: true,
            summary: truncate(&summary.into(), 16_000),
            metadata,
            error: None,
            sandbox_violation: false,
            duration_ms: 0,
        }
    }

    fn fail(error: impl Into<String>, metadata: Value, violation: bool) -> Self {
        let scrubbed = lmhub_core::redact::scrub(&error.into());
        Self {
            success: false,
            summary: format!("ERROR: {scrubbed}"),
            metadata,
            error: Some(scrubbed.clone()),
            sandbox_violation: violation,
            duration_ms: 0,
        }
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars.saturating_sub(20)).collect();
        format!("{head}\n[...output truncated...]")
    }
}

pub struct ToolRuntime {
    jail: PathJail,
    home_dir: PathBuf,
    tmp_dir: PathBuf,
    config: SandboxConfig,
    last_capture: Mutex<Option<(PathBuf, PathBuf)>>,
}

impl ToolRuntime {
    /// Create the runtime for one run. Hidden `.home` / `.tmp` dirs live
    /// inside the workspace so child processes get writable HOME/TMPDIR
    /// without touching the real user home.
    pub fn create(workspace_root: &std::path::Path, config: SandboxConfig) -> Result<Self> {
        let jail = PathJail::create(workspace_root)?;
        let home_dir = jail.root().join(".home");
        let tmp_dir = jail.root().join(".tmp");
        std::fs::create_dir_all(&home_dir)?;
        std::fs::create_dir_all(&tmp_dir)?;
        Ok(Self {
            jail,
            home_dir,
            tmp_dir,
            config,
            last_capture: Mutex::new(None),
        })
    }

    pub fn root(&self) -> &std::path::Path {
        self.jail.root()
    }

    pub async fn execute(&self, name: &str, args: &Value) -> ToolOutcome {
        let started = std::time::Instant::now();
        let mut outcome = self.dispatch(name, args).await;
        outcome.duration_ms = started.elapsed().as_millis() as u64;
        if outcome.duration_ms == 0 {
            outcome.duration_ms = 1; // events expect > 0 granularity
        }
        outcome
    }

    async fn dispatch(&self, name: &str, args: &Value) -> ToolOutcome {
        match name {
            "list_directory" => self.list_directory(args).await,
            "read_file" => self.read_file(args).await,
            "write_file" => self.write_file(args).await,
            "edit_file" => self.edit_file(args).await,
            "create_directory" => self.create_directory(args).await,
            "run_command" => self.run_command(args).await,
            "read_command_output" => self.read_command_output().await,
            other => ToolOutcome::fail(
                format!("unknown tool {other:?}; allowed: {TOOL_NAMES:?}"),
                json!({}),
                false,
            ),
        }
    }

    fn resolve(&self, raw: &str) -> std::result::Result<PathBuf, CoreError> {
        self.jail.resolve(raw)
    }

    async fn list_directory(&self, args: &Value) -> ToolOutcome {
        let raw = arg_str(args, "path").unwrap_or_else(|| ".".to_string());
        let path = match self.resolve(&raw) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        let mut rd = match tokio::fs::read_dir(&path).await {
            Ok(rd) => rd,
            Err(e) => {
                return ToolOutcome::fail(format!("cannot list {raw:?}: {e}"), json!({}), false)
            }
        };
        loop {
            match rd.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                        dirs.push(format!("{name}/"));
                    } else {
                        let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                        files.push(format!("{name} ({size} bytes)"));
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    return ToolOutcome::fail(
                        format!("readdir failed on {raw:?}: {e}"),
                        json!({}),
                        false,
                    )
                }
            }
        }
        dirs.sort();
        files.sort();
        const CAP: usize = 500;
        let mut lines = Vec::new();
        let total = dirs.len() + files.len();
        lines.extend(dirs.iter().take(CAP).cloned());
        lines.extend(files.iter().take(CAP.saturating_sub(dirs.len())).cloned());
        if total > CAP {
            lines.push(format!("[...{total} entries total, showing {CAP}...]"));
        }
        let listing = if lines.is_empty() {
            "(empty directory)".to_string()
        } else {
            lines.join("\n")
        };
        ToolOutcome::ok(
            listing,
            json!({"path": raw, "dirs": dirs.len(), "files": files.len()}),
        )
    }

    async fn read_file(&self, args: &Value) -> ToolOutcome {
        let Some(raw) = arg_str(args, "path") else {
            return ToolOutcome::fail("missing required argument `path`", json!({}), false);
        };
        let path = match self.resolve(&raw) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        let requested_cap = args.get("max_bytes").and_then(|v| v.as_u64());
        let cap = requested_cap
            .unwrap_or(self.config.read_file_max_bytes)
            .min(self.config.read_file_max_bytes.max(1));
        let data = match tokio::fs::read(&path).await {
            Ok(d) => d,
            Err(e) => {
                return ToolOutcome::fail(format!("cannot read {raw:?}: {e}"), json!({}), false)
            }
        };
        let truncated_by_size = data.len() as u64 > cap;
        let slice = &data[..(cap as usize).min(data.len())];
        let mut text = String::from_utf8_lossy(slice).to_string();
        let offset_line = args
            .get("offset_line")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        if offset_line > 0 {
            let skipped = text.lines().count() <= offset_line;
            text = text
                .lines()
                .skip(offset_line)
                .collect::<Vec<_>>()
                .join("\n");
            if skipped {
                return ToolOutcome::fail(
                    format!("offset_line {offset_line} beyond end of file"),
                    json!({"path": raw}),
                    false,
                );
            }
        }
        let meta = json!({
            "path": raw,
            "bytes_read": slice.len(),
            "truncated": truncated_by_size,
            "offset_line": offset_line,
        });
        let out = if truncated_by_size {
            format!(
                "{text}\n[...file truncated at {cap} bytes — use offset_line/max_bytes to page...]"
            )
        } else {
            text
        };
        ToolOutcome::ok(out, meta)
    }

    async fn write_file(&self, args: &Value) -> ToolOutcome {
        let Some(raw) = arg_str(args, "path") else {
            return ToolOutcome::fail("missing required argument `path`", json!({}), false);
        };
        let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
            return ToolOutcome::fail(
                "missing required string argument `content`",
                json!({}),
                false,
            );
        };
        if content.len() as u64 > self.config.write_file_max_bytes {
            return ToolOutcome::fail(
                format!(
                    "content too large ({} bytes > cap {})",
                    content.len(),
                    self.config.write_file_max_bytes
                ),
                json!({"path": raw}),
                false,
            );
        }
        let path = match self.resolve(&raw) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolOutcome::fail(
                    format!("cannot create parent directories for {raw:?}: {e}"),
                    json!({"path": raw}),
                    false,
                );
            }
        }
        match tokio::fs::write(&path, content.as_bytes()).await {
            Ok(()) => ToolOutcome::ok(
                format!("wrote {} bytes to {}", content.len(), raw),
                json!({"path": raw, "bytes": content.len(), "created_or_overwritten": true}),
            ),
            Err(e) => ToolOutcome::fail(
                format!("cannot write {raw:?}: {e}"),
                json!({"path": raw}),
                false,
            ),
        }
    }

    async fn edit_file(&self, args: &Value) -> ToolOutcome {
        let Some(raw) = arg_str(args, "path") else {
            return ToolOutcome::fail("missing required argument `path`", json!({}), false);
        };
        let Some(old) = args.get("old_string").and_then(|v| v.as_str()) else {
            return ToolOutcome::fail(
                "missing required string argument `old_string`",
                json!({}),
                false,
            );
        };
        let new = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let path = match self.resolve(&raw) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        let current = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                return ToolOutcome::fail(
                    format!("cannot read {raw:?} for editing (utf-8 text only): {e}"),
                    json!({"path": raw}),
                    false,
                )
            }
        };
        let occurrences = current.matches(old).count();
        if occurrences == 0 {
            return ToolOutcome::fail(
                format!(
                    "old_string not found in {raw:?}; read the file first and copy the exact text"
                ),
                json!({"path": raw}),
                false,
            );
        }
        if !replace_all && occurrences > 1 {
            return ToolOutcome::fail(
                format!(
                    "old_string matches {occurrences} times in {raw:?}; provide more context to make it unique, or set replace_all=true"
                ),
                json!({"path": raw, "occurrences": occurrences}),
                false,
            );
        }
        let updated = if replace_all {
            current.replace(old, new)
        } else {
            current.replacen(old, new, 1)
        };
        if updated.len() as u64 > self.config.write_file_max_bytes {
            return ToolOutcome::fail(
                "edited file would exceed size cap".to_string(),
                json!({"path": raw}),
                false,
            );
        }
        match tokio::fs::write(&path, updated.as_bytes()).await {
            Ok(()) => ToolOutcome::ok(
                format!("edited {raw:?} ({} replacement)", occurrences.min(1)),
                json!({"path": raw, "replacements": if replace_all { occurrences } else { 1 }}),
            ),
            Err(e) => ToolOutcome::fail(format!("cannot write {raw:?}: {e}"), json!({}), false),
        }
    }

    async fn create_directory(&self, args: &Value) -> ToolOutcome {
        let Some(raw) = arg_str(args, "path") else {
            return ToolOutcome::fail("missing required argument `path`", json!({}), false);
        };
        let path = match self.resolve(&raw) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        match tokio::fs::create_dir_all(&path).await {
            Ok(()) => ToolOutcome::ok(format!("directory {raw:?} ready"), json!({"path": raw})),
            Err(e) => ToolOutcome::fail(
                format!("cannot create directory {raw:?}: {e}"),
                json!({"path": raw}),
                false,
            ),
        }
    }

    async fn run_command(&self, args: &Value) -> ToolOutcome {
        let Some(argv): Option<Vec<String>> =
            args.get("argv").and_then(|v| v.as_array()).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
        else {
            return ToolOutcome::fail(
                "missing required argument `argv` (array of strings)".to_string(),
                json!({}),
                false,
            );
        };
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.config.command_timeout.as_secs())
            .clamp(5, 300);
        let id = format!("cmd-{}", uuid::Uuid::new_v4().simple());
        let result = sandbox_proc::run_allowlisted(
            &argv,
            &self.config.allowed_commands,
            self.jail.root(),
            &self.tmp_dir,
            &self.home_dir,
            Duration::from_secs(timeout_secs),
            &id,
        )
        .await;

        // Persist capture paths even when the command itself failed.
        let (stdout_path, stderr_path) = match &result {
            Ok(out) => (Some(out.stdout_path.clone()), Some(out.stderr_path.clone())),
            Err(_) => (None, None),
        };

        let meta_argv = json!(argv);
        match result {
            Ok(out) => {
                *self.last_capture.lock().unwrap() =
                    Some((out.stdout_path.clone(), out.stderr_path.clone()));
                let stdout_tail = sandbox_proc::read_capture_tail(&out.stdout_path, 12_000);
                let stderr_tail = sandbox_proc::read_capture_tail(&out.stderr_path, 6_000);
                let status_label = if out.timed_out { "timeout" } else { "exit" };
                let code_desc = out
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into());
                let mut summary =
                    format!(
                    "[{status_label} {code_desc}, {} ms]\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    out.duration_ms, stdout_tail, stderr_tail.trim_end()
                );
                if out.timed_out {
                    summary.push_str(&format!(
                        "\n[command exceeded {timeout_secs}s and was killed]"
                    ));
                }
                ToolOutcome {
                    success: out.success(),
                    summary: truncate(&summary, 20_000),
                    metadata: json!({
                        "argv": meta_argv,
                        "exit_code": out.exit_code,
                        "timed_out": out.timed_out,
                        "duration_ms": out.duration_ms,
                        "stdout_bytes": out.stdout_bytes,
                        "stderr_bytes": out.stderr_bytes,
                    }),
                    error: None,
                    sandbox_violation: false,
                    duration_ms: out.duration_ms,
                }
            }
            Err(e @ CoreError::Sandbox(_)) => {
                let _ = (stdout_path, stderr_path);
                ToolOutcome {
                    success: false,
                    summary: format!("BLOCKED: {}", e),
                    metadata: json!({"argv": meta_argv, "blocked": true}),
                    error: Some(e.to_string()),
                    sandbox_violation: true,
                    duration_ms: 0,
                }
            }
            Err(e) => ToolOutcome::fail(
                format!("run_command failed: {e}"),
                json!({"argv": meta_argv}),
                false,
            ),
        }
    }

    async fn read_command_output(&self) -> ToolOutcome {
        let capture = self.last_capture.lock().unwrap().clone();
        let Some((stdout, stderr)) = capture else {
            return ToolOutcome::fail(
                "no command output yet — run_command first".to_string(),
                json!({}),
                false,
            );
        };
        let stdout_tail = sandbox_proc::read_capture_tail(&stdout, 16_000);
        let stderr_tail = sandbox_proc::read_capture_tail(&stderr, 8_000);
        ToolOutcome::ok(
            format!(
                "--- stdout ---\n{}\n--- stderr ---\n{}",
                stdout_tail,
                stderr_tail.trim_end()
            ),
            json!({}),
        )
    }
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)?.as_str().map(|s| s.to_string())
}

fn violation_outcome(e: CoreError) -> ToolOutcome {
    ToolOutcome {
        success: false,
        summary: format!("SANDBOX VIOLATION: {e}"),
        metadata: json!({"violation": true}),
        error: Some(e.to_string()),
        sandbox_violation: true,
        duration_ms: 0,
    }
}
