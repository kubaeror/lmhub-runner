//! The controlled tools handed to the model. Every file operation is
//! jailed to the workspace; every command runs allowlisted and time-boxed.

use crate::jail::PathJail;
use crate::proc as sandbox_proc;
use lmhub_core::{CoreError, Result, ToolSpec};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub const TOOL_NAMES: [&str; 15] = [
    "list_directory",
    "read_file",
    "read_files",
    "write_file",
    "append_file",
    "edit_file",
    "create_directory",
    "move_file",
    "copy_file",
    "get_file_info",
    "find_files",
    "search_files",
    "read_workspace_tree",
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
            name: "read_files".into(),
            description: "Read the first page of up to 10 files in one call (per-file cap applies; \
                          use read_file for paging). Failing files are reported inline without \
                          failing the batch.\
                          \nExample: {\"paths\": [\"package.json\", \"README.md\"]}"
                .into(),
            parameters: obj(json!({
                "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 10,
                          "description": "File paths, relative to workspace."},
                "max_bytes": {"type": "integer", "description": "Per-file byte cap (default 8000; server cap applies)."}
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
            name: "append_file".into(),
            description: "Append text to a file (created if missing). Size-capped like write_file. \
                          By default a newline is inserted first when the file does not end with \
                          one, so chunks stay line-aligned.\
                          \nExample: {\"path\": \"CHANGELOG.md\", \"content\": \"- new feature\\n\"}"
                .into(),
            parameters: obj(json!({
                "path": {"type": "string", "description": "File path relative to workspace."},
                "content": {"type": "string", "description": "Text to append."},
                "ensure_newline": {"type": "boolean", "description": "Insert a newline first when missing (default true)."}
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
            name: "move_file".into(),
            description: "Move (rename) a file or directory inside your workspace; destination \
                          parent directories are created. Both paths are jail-checked.\
                          \nExample: {\"source\": \"src/old.rs\", \"destination\": \"src/lib/old.rs\"}"
                .into(),
            parameters: obj(json!({
                "source": {"type": "string", "description": "Path relative to workspace."},
                "destination": {"type": "string", "description": "New path relative to workspace."}
            })),
        },
        ToolSpec {
            name: "copy_file".into(),
            description: "Copy a file inside your workspace; destination parent directories are \
                          created. Source size must be within the write cap.\
                          \nExample: {\"source\": \"a.js\", \"destination\": \"b.js\"}"
                .into(),
            parameters: obj(json!({
                "source": {"type": "string", "description": "Path relative to workspace."},
                "destination": {"type": "string", "description": "New path relative to workspace."}
            })),
        },
        ToolSpec {
            name: "get_file_info".into(),
            description: "Metadata of one file or directory: type, size in bytes, last-modified \
                          (unix seconds) and unix permissions (octal).\
                          \nExample: {\"path\": \"package.json\"}"
                .into(),
            parameters: obj(json!({
                "path": {"type": "string"}
            })),
        },
        ToolSpec {
            name: "find_files".into(),
            description: "Find files by name pattern (globs: `*` matches any run of characters, `?` \
                          matches one; `*` crosses path separators, so `**/*.test.ts` works). \
                          Returns workspace-relative paths, sorted.\
                          \nExample: {\"pattern\": \"**/*.test.ts\"}"
                .into(),
            parameters: obj(json!({
                "pattern": {"type": "string", "description": "Glob matched against workspace-relative paths."},
                "path": {"type": "string", "description": "Directory to search (default \".\")."},
                "max_results": {"type": "integer", "description": "Cap on returned paths (default 200, max 1000)."}
            })),
        },
        ToolSpec {
            name: "search_files".into(),
            description: "Search file contents for a substring (not a regex). Files are scanned up \
                          to 64 KB each; binary files are skipped. Results are `path:line: content` \
                          lines, sorted by path. Prefer this over run_command grep for content \
                          queries.\
                          \nExample: {\"pattern\": \"TODO\", \"path\": \"src\", \"case_insensitive\": true, \"max_results\": 50}"
                .into(),
            parameters: obj(json!({
                "pattern": {"type": "string", "description": "Substring to search for."},
                "path": {"type": "string", "description": "Directory to search (default \".\")."},
                "case_insensitive": {"type": "boolean", "description": "Ignore case (default false)."},
                "max_results": {"type": "integer", "description": "Cap on returned matches (default 100, max 500)."}
            })),
        },
        ToolSpec {
            name: "read_workspace_tree".into(),
            description: "Recursive directory tree of your workspace (files with sizes, directories \
                          with trailing `/`), depth- and entry-capped. Cheap orientation before \
                          deeper exploration.\
                          \nExample: {\"depth\": 2}"
                .into(),
            parameters: obj(json!({
                "path": {"type": "string", "description": "Directory to start from (default \".\")."},
                "depth": {"type": "integer", "description": "Levels below the start to descend (default 2, max 5)."},
                "max_entries": {"type": "integer", "description": "Cap on listed entries (default 200, max 1000)."}
            })),
        },
        ToolSpec {
            name: "run_command".into(),
            description: "Run an ALLOWLISTED command inside the workspace. Only specific \
                          commands are available (e.g. node, npm, git, grep, python3); arbitrary \
                          binaries and shells are rejected. Pass argv as an array of strings — no \
                          shell syntax, no pipes/redirects. The command id is reported in the \
                          summary; retrieve full output later with read_command_output.\
                          \nExample: {\"argv\": [\"node\", \"--version\"]}"
                .into(),
            parameters: obj(json!({
                "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                         "description": "Command and arguments, e.g. [\"npm\", \"init\", \"-y\"]"},
                "cwd": {"type": "string", "description": "Working directory for the command, relative to workspace (default \".\")."},
                "timeout_secs": {"type": "integer", "description": "Optional timeout 5..=300 (server default applies)."}
            })),
        },
        ToolSpec {
            name: "read_command_output".into(),
            description: "Read captured stdout/stderr of a recent run_command call. Omit \
                          command_id for the most recent output; the last 5 commands are retained."
                .into(),
            parameters: obj(json!({
                "command_id": {"type": "string", "description": "Optional id reported by run_command (default: most recent)."}
            })),
        },
    ]
}

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub allowed_commands: Vec<String>,
    pub command_timeout: Duration,
    pub read_file_max_bytes: u64,
    pub write_file_max_bytes: u64,
    /// OS-level isolation backend for `run_command`.
    pub runtime: crate::runtime::SandboxRuntime,
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

/// One retained command capture. Capture files live in the run's `.tmp`
/// directory, so they die with the run — retention is just bookkeeping.
#[derive(Debug, Clone)]
struct CommandCapture {
    id: String,
    stdout: PathBuf,
    stderr: PathBuf,
}

/// How many recent command captures `read_command_output` can fetch.
const OUTPUT_RETENTION: usize = 5;

pub struct ToolRuntime {
    jail: PathJail,
    home_dir: PathBuf,
    tmp_dir: PathBuf,
    config: SandboxConfig,
    command_outputs: Mutex<std::collections::VecDeque<CommandCapture>>,
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
            command_outputs: Mutex::new(std::collections::VecDeque::new()),
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
            "read_files" => self.read_files(args).await,
            "write_file" => self.write_file(args).await,
            "append_file" => self.append_file(args).await,
            "edit_file" => self.edit_file(args).await,
            "create_directory" => self.create_directory(args).await,
            "move_file" => self.move_file(args).await,
            "copy_file" => self.copy_file(args).await,
            "get_file_info" => self.get_file_info(args).await,
            "find_files" => self.find_files(args).await,
            "search_files" => self.search_files(args).await,
            "read_workspace_tree" => self.read_workspace_tree(args).await,
            "run_command" => self.run_command(args).await,
            "read_command_output" => self.read_command_output(args).await,
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
        let offset_line = args
            .get("offset_line")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                return ToolOutcome::fail(format!("cannot read {raw:?}: {e}"), json!({}), false)
            }
        };
        // Stream the file in bounded chunks: skip `offset_line` lines first,
        // then collect up to `cap` bytes. Memory use is bounded by the cap
        // regardless of file size, and paging works against the whole file.
        let mut out: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut lines_remaining = offset_line;
        let mut collecting = offset_line == 0;
        let mut truncated = false;
        let mut newlines_seen: u64 = 0;
        let mut non_empty = false;
        let mut ends_with_newline = false;
        loop {
            let n = match file.read(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    return ToolOutcome::fail(format!("cannot read {raw:?}: {e}"), json!({}), false)
                }
            };
            if n == 0 {
                break;
            }
            let chunk = &buf[..n];
            non_empty = true;
            ends_with_newline = chunk[n - 1] == b'\n';
            newlines_seen += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
            if !collecting {
                let mut start = 0usize;
                for (i, &b) in chunk.iter().enumerate() {
                    if b == b'\n' {
                        lines_remaining -= 1;
                        if lines_remaining == 0 {
                            start = i + 1;
                            collecting = true;
                            break;
                        }
                    }
                }
                if !collecting {
                    continue;
                }
                let take = (cap as usize - out.len()).min(chunk.len() - start);
                out.extend_from_slice(&chunk[start..start + take]);
                if take < chunk.len() - start {
                    truncated = true;
                    break;
                }
            } else {
                let take = (cap as usize - out.len()).min(chunk.len());
                out.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    truncated = true;
                    break;
                }
            }
        }
        // Matches `lines().count() <= offset_line`: a trailing newline does
        // not count as an extra line.
        let total_lines = newlines_seen + u64::from(non_empty && !ends_with_newline);
        if offset_line > 0 && total_lines <= offset_line {
            return ToolOutcome::fail(
                format!("offset_line {offset_line} beyond end of file"),
                json!({"path": raw}),
                false,
            );
        }
        let text = String::from_utf8_lossy(&out).to_string();
        let meta = json!({
            "path": raw,
            "bytes_read": out.len(),
            "truncated": truncated,
            "offset_line": offset_line,
        });
        let out_text = if truncated {
            format!(
                "{text}\n[...file truncated at {cap} bytes — use offset_line/max_bytes to page...]"
            )
        } else {
            text
        };
        ToolOutcome::ok(out_text, meta)
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
        // Cap the read so a huge file cannot exhaust memory; edits are meant
        // for small snippets anyway.
        const EDIT_MAX_BYTES: u64 = 8 * 1024 * 1024;
        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                return ToolOutcome::fail(
                    format!("cannot read {raw:?} for editing (utf-8 text only): {e}"),
                    json!({"path": raw}),
                    false,
                )
            }
        };
        let mut bytes = Vec::new();
        if let Err(e) = file.take(EDIT_MAX_BYTES + 1).read_to_end(&mut bytes).await {
            return ToolOutcome::fail(
                format!("cannot read {raw:?} for editing: {e}"),
                json!({"path": raw}),
                false,
            );
        }
        if bytes.len() as u64 > EDIT_MAX_BYTES {
            return ToolOutcome::fail(
                format!("file too large to edit (limit {EDIT_MAX_BYTES} bytes); use write_file with full content instead"),
                json!({"path": raw}),
                false,
            );
        }
        let current = match String::from_utf8(bytes) {
            Ok(c) => c,
            Err(_) => {
                return ToolOutcome::fail(
                    format!("cannot read {raw:?} for editing (utf-8 text only)"),
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

    // ---- files ------------------------------------------------------------

    async fn move_file(&self, args: &Value) -> ToolOutcome {
        let Some(src) = arg_str(args, "source") else {
            return ToolOutcome::fail("missing required argument `source`", json!({}), false);
        };
        let Some(dst) = arg_str(args, "destination") else {
            return ToolOutcome::fail("missing required argument `destination`", json!({}), false);
        };
        let src_path = match self.resolve(&src) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        let dst_path = match self.resolve(&dst) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        if let Some(parent) = dst_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolOutcome::fail(
                    format!("cannot create parent directories for {dst:?}: {e}"),
                    json!({"source": src, "destination": dst}),
                    false,
                );
            }
        }
        match tokio::fs::rename(&src_path, &dst_path).await {
            Ok(()) => ToolOutcome::ok(
                format!("moved {src:?} → {dst:?}"),
                json!({"source": src, "destination": dst}),
            ),
            Err(e) => ToolOutcome::fail(
                format!("cannot move {src:?} → {dst:?}: {e}"),
                json!({"source": src, "destination": dst}),
                false,
            ),
        }
    }

    async fn copy_file(&self, args: &Value) -> ToolOutcome {
        let Some(src) = arg_str(args, "source") else {
            return ToolOutcome::fail("missing required argument `source`", json!({}), false);
        };
        let Some(dst) = arg_str(args, "destination") else {
            return ToolOutcome::fail("missing required argument `destination`", json!({}), false);
        };
        let src_path = match self.resolve(&src) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        let dst_path = match self.resolve(&dst) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        let size = match std::fs::metadata(&src_path) {
            Ok(m) => m.len(),
            Err(e) => {
                return ToolOutcome::fail(
                    format!("cannot stat {src:?}: {e}"),
                    json!({"source": src, "destination": dst}),
                    false,
                )
            }
        };
        if size > self.config.write_file_max_bytes {
            return ToolOutcome::fail(
                format!(
                    "source {src:?} too large ({size} bytes > cap {})",
                    self.config.write_file_max_bytes
                ),
                json!({"source": src, "destination": dst}),
                false,
            );
        }
        if let Some(parent) = dst_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolOutcome::fail(
                    format!("cannot create parent directories for {dst:?}: {e}"),
                    json!({"source": src, "destination": dst}),
                    false,
                );
            }
        }
        match tokio::fs::copy(&src_path, &dst_path).await {
            Ok(n) => ToolOutcome::ok(
                format!("copied {src:?} → {dst:?} ({n} bytes)"),
                json!({"source": src, "destination": dst, "bytes": n}),
            ),
            Err(e) => ToolOutcome::fail(
                format!("cannot copy {src:?} → {dst:?}: {e}"),
                json!({"source": src, "destination": dst}),
                false,
            ),
        }
    }

    async fn get_file_info(&self, args: &Value) -> ToolOutcome {
        let Some(raw) = arg_str(args, "path") else {
            return ToolOutcome::fail("missing required argument `path`", json!({}), false);
        };
        let path = match self.resolve(&raw) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                return ToolOutcome::fail(
                    format!("cannot stat {raw:?}: {e}"),
                    json!({"path": raw}),
                    false,
                )
            }
        };
        let kind = if meta.is_dir() { "directory" } else { "file" };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(meta.permissions().mode() & 0o777)
        };
        #[cfg(not(unix))]
        let mode: Option<u32> = None;
        ToolOutcome::ok(
            format!(
                "type: {kind}\nsize: {} bytes\nmodified: {} (unix seconds)\nmode: {} (octal)",
                meta.len(),
                modified
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "?".into()),
                mode.map(|m| format!("{m:o}")).unwrap_or_else(|| "?".into()),
            ),
            json!({
                "path": raw,
                "kind": kind,
                "size": meta.len(),
                "modified_secs": modified,
                "mode": mode,
            }),
        )
    }

    async fn append_file(&self, args: &Value) -> ToolOutcome {
        let Some(raw) = arg_str(args, "path") else {
            return ToolOutcome::fail("missing required argument `path`", json!({}), false);
        };
        let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
            return ToolOutcome::fail(
                "missing required string argument `content`",
                json!({"path": raw}),
                false,
            );
        };
        let ensure_newline = args
            .get("ensure_newline")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
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
        let existing = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let ends_nl = if existing == 0 {
            true
        } else {
            let mut f = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) => {
                    return ToolOutcome::fail(
                        format!("cannot open {raw:?} for append: {e}"),
                        json!({"path": raw}),
                        false,
                    )
                }
            };
            let mut byte = [0u8; 1];
            f.seek(SeekFrom::End(-1)).is_ok() && f.read_exact(&mut byte).is_ok() && byte[0] == b'\n'
        };
        let needs_newline = ensure_newline && !ends_nl;
        let added = content.len() as u64 + if needs_newline { 1 } else { 0 };
        if existing + added > self.config.write_file_max_bytes {
            return ToolOutcome::fail(
                format!(
                    "append would exceed size cap ({} + {added} > {})",
                    existing, self.config.write_file_max_bytes
                ),
                json!({"path": raw}),
                false,
            );
        }
        let mut f = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                return ToolOutcome::fail(
                    format!("cannot open {raw:?} for append: {e}"),
                    json!({"path": raw}),
                    false,
                )
            }
        };
        if needs_newline && f.write_all(b"\n").is_err() {
            return ToolOutcome::fail(
                format!("cannot append to {raw:?}: write failed"),
                json!({"path": raw}),
                false,
            );
        }
        if let Err(e) = f.write_all(content.as_bytes()) {
            return ToolOutcome::fail(
                format!("cannot append to {raw:?}: {e}"),
                json!({"path": raw}),
                false,
            );
        }
        ToolOutcome::ok(
            format!(
                "appended {} bytes to {} (new size {})",
                content.len(),
                raw,
                existing + added
            ),
            json!({"path": raw, "bytes": content.len(), "new_size": existing + added}),
        )
    }

    async fn read_files(&self, args: &Value) -> ToolOutcome {
        let Some(paths) = args.get("paths").and_then(|v| v.as_array()) else {
            return ToolOutcome::fail("missing required array argument `paths`", json!({}), false);
        };
        if paths.is_empty() || paths.len() > 10 {
            return ToolOutcome::fail(
                "`paths` must contain between 1 and 10 entries".to_string(),
                json!({"count": paths.len()}),
                false,
            );
        }
        let cap = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(8_000)
            .min(self.config.read_file_max_bytes.max(1));
        let mut out: Vec<String> = Vec::new();
        let mut failed = 0usize;
        for p in paths {
            let Some(raw) = p.as_str() else {
                out.push("=== <non-string path> ===\nERROR: non-string entry in paths".into());
                failed += 1;
                continue;
            };
            out.push(format!("=== {raw} ==="));
            let path = match self.resolve(raw) {
                Ok(p) => p,
                Err(e) => {
                    out.push(format!("ERROR: {e}"));
                    failed += 1;
                    continue;
                }
            };
            match read_head(&path, cap) {
                Ok(text) => out.push(text),
                Err(e) => {
                    out.push(format!("ERROR: {e}"));
                    failed += 1;
                }
            }
        }
        if failed > 0 {
            out.push(format!("[{failed} file(s) could not be read]"));
        }
        ToolOutcome::ok(
            out.join("\n"),
            json!({"files": paths.len(), "failed": failed, "per_file_cap_bytes": cap}),
        )
    }

    // ---- search -----------------------------------------------------------

    async fn find_files(&self, args: &Value) -> ToolOutcome {
        let Some(pattern) = arg_str(args, "pattern") else {
            return ToolOutcome::fail("missing required argument `pattern`", json!({}), false);
        };
        if pattern.len() > 500 {
            return ToolOutcome::fail(
                "pattern too long (max 500 chars)".to_string(),
                json!({}),
                false,
            );
        }
        let raw_root = arg_str(args, "path").unwrap_or_else(|| ".".to_string());
        let root = match self.resolve(&raw_root) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        if !root.is_dir() {
            return ToolOutcome::fail(
                format!("{raw_root:?} is not a directory"),
                json!({"path": raw_root}),
                false,
            );
        }
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(200)
            .clamp(1, 1000) as usize;
        // Output paths are workspace-relative (copy-paste into read_file),
        // even when searching a subdirectory.
        let root_rel = root
            .strip_prefix(self.jail.root())
            .unwrap_or(Path::new(""))
            .to_path_buf();
        let mut found: Vec<String> = Vec::new();
        for rel in walk_relative(&root, 12, 10_000) {
            if found.len() >= max_results {
                break;
            }
            let rel_str = rel.to_string_lossy().to_string();
            if glob_match(&pattern, &rel_str) {
                found.push(display_rel(&root_rel, &rel));
            }
        }
        let mut summary = if found.is_empty() {
            format!("no files match {pattern:?} under {raw_root}")
        } else {
            found.join("\n")
        };
        if found.len() >= max_results {
            summary.push_str(&format!("\n[...showing first {max_results} matches...]"));
        }
        ToolOutcome::ok(summary, json!({"pattern": pattern, "matches": found.len()}))
    }

    async fn search_files(&self, args: &Value) -> ToolOutcome {
        let Some(pattern) = arg_str(args, "pattern") else {
            return ToolOutcome::fail("missing required argument `pattern`", json!({}), false);
        };
        if pattern.len() > 500 {
            return ToolOutcome::fail(
                "pattern too long (max 500 chars)".to_string(),
                json!({}),
                false,
            );
        }
        let raw_root = arg_str(args, "path").unwrap_or_else(|| ".".to_string());
        let root = match self.resolve(&raw_root) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        if !root.is_dir() {
            return ToolOutcome::fail(
                format!("{raw_root:?} is not a directory"),
                json!({"path": raw_root}),
                false,
            );
        }
        let case_insensitive = args
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(100)
            .clamp(1, 500) as usize;
        let needle = if case_insensitive {
            pattern.to_lowercase()
        } else {
            pattern.clone()
        };
        const PER_FILE: u64 = 64 * 1024;
        // Output paths are workspace-relative (copy-paste into read_file),
        // even when searching a subdirectory.
        let root_rel = root
            .strip_prefix(self.jail.root())
            .unwrap_or(Path::new(""))
            .to_path_buf();
        let mut matches: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        let mut cut_files = 0usize;
        for rel in walk_relative(&root, 12, 10_000) {
            if matches.len() >= max_results {
                break;
            }
            let abs = root.join(&rel);
            let Ok(meta) = std::fs::metadata(&abs) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let Ok(f) = std::fs::File::open(&abs) else {
                continue;
            };
            let mut bytes = Vec::new();
            if f.take(PER_FILE).read_to_end(&mut bytes).is_err() {
                continue;
            }
            if bytes[..bytes.len().min(8192)].contains(&0) {
                continue; // binary file (NUL in the head)
            }
            if meta.len() > PER_FILE {
                cut_files += 1; // scanned only the first 64 KB
            }
            scanned += 1;
            let rel_str = display_rel(&root_rel, &rel);
            for (i, line) in String::from_utf8_lossy(&bytes).split('\n').enumerate() {
                if matches.len() >= max_results {
                    break;
                }
                let hay = if case_insensitive {
                    line.to_lowercase()
                } else {
                    line.to_string()
                };
                if hay.contains(&needle) {
                    let shown: String = line.chars().take(200).collect();
                    matches.push(format!("{rel_str}:{}: {shown}", i + 1));
                }
            }
        }
        let mut summary = if matches.is_empty() {
            format!("no matches for {pattern:?} under {raw_root}")
        } else {
            matches.join("\n")
        };
        if matches.len() >= max_results {
            summary.push_str(&format!("\n[...showing first {max_results} matches...]"));
        }
        if cut_files > 0 {
            summary.push_str(&format!(
                "\n[{cut_files} file(s) larger than 64 KB scanned partially]"
            ));
        }
        ToolOutcome::ok(
            summary,
            json!({"pattern": pattern, "matches": matches.len(), "files_scanned": scanned}),
        )
    }

    async fn read_workspace_tree(&self, args: &Value) -> ToolOutcome {
        let raw_root = arg_str(args, "path").unwrap_or_else(|| ".".to_string());
        let root = match self.resolve(&raw_root) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        if !root.is_dir() {
            return ToolOutcome::fail(
                format!("{raw_root:?} is not a directory"),
                json!({"path": raw_root}),
                false,
            );
        }
        let max_depth = args
            .get("depth")
            .and_then(|v| v.as_u64())
            .unwrap_or(2)
            .clamp(0, 5) as usize;
        let max_entries = args
            .get("max_entries")
            .and_then(|v| v.as_u64())
            .unwrap_or(200)
            .clamp(1, 1000) as usize;
        let rels = walk_relative(&root, max_depth, max_entries + 1);
        let truncated = rels.len() > max_entries;
        let mut lines: Vec<String> = Vec::new();
        for rel in rels.iter().take(max_entries) {
            let indent = "  ".repeat(rel.components().count().saturating_sub(1));
            let name = rel
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let abs = root.join(rel);
            if abs.is_dir() {
                lines.push(format!("{indent}{name}/"));
            } else {
                let size = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
                lines.push(format!("{indent}{name} ({size} bytes)"));
            }
        }
        if truncated {
            lines.push(format!("[...more than {max_entries} entries...]"));
        }
        ToolOutcome::ok(
            if lines.is_empty() {
                format!("(empty directory {raw_root})")
            } else {
                lines.join("\n")
            },
            json!({"path": raw_root, "entries": rels.len().min(max_entries), "truncated": truncated}),
        )
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
        let cwd_raw = arg_str(args, "cwd").unwrap_or_else(|| ".".to_string());
        let cwd = match self.resolve(&cwd_raw) {
            Ok(p) => p,
            Err(e) => return violation_outcome(e),
        };
        if !cwd.is_dir() {
            return ToolOutcome::fail(
                format!("cwd {cwd_raw:?} is not a directory"),
                json!({"argv": argv}),
                false,
            );
        }
        let id = format!("cmd-{}", uuid::Uuid::new_v4().simple());
        let result = sandbox_proc::run_allowlisted(
            &argv,
            &self.config.allowed_commands,
            self.jail.root(),
            &cwd,
            &self.tmp_dir,
            &self.home_dir,
            Duration::from_secs(timeout_secs),
            &id,
            &self.config.runtime,
        )
        .await;

        let meta_argv = json!(argv);
        let meta_cwd = json!(cwd_raw);
        match result {
            Ok(out) => {
                // Retain the capture for read_command_output (last N kept).
                {
                    let mut ring = self.command_outputs.lock().unwrap();
                    ring.push_back(CommandCapture {
                        id: id.clone(),
                        stdout: out.stdout_path.clone(),
                        stderr: out.stderr_path.clone(),
                    });
                    while ring.len() > OUTPUT_RETENTION {
                        ring.pop_front();
                    }
                }
                let stdout_tail = sandbox_proc::read_capture_tail(&out.stdout_path, 12_000);
                let stderr_tail = sandbox_proc::read_capture_tail(&out.stderr_path, 6_000);
                let status_label = if out.timed_out { "timeout" } else { "exit" };
                let code_desc = out
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into());
                let mut summary = format!(
                    "[{status_label} {code_desc}, {} ms] (id: {id})\n--- stdout ---\n{}\n--- stderr ---\n{}",
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
                        "cwd": meta_cwd,
                        "command_id": id,
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
            Err(e @ CoreError::Sandbox(_)) => ToolOutcome {
                success: false,
                summary: format!("BLOCKED: {}", e),
                metadata: json!({"argv": meta_argv, "cwd": meta_cwd, "blocked": true}),
                error: Some(e.to_string()),
                sandbox_violation: true,
                duration_ms: 0,
            },
            Err(e) => ToolOutcome::fail(
                format!("run_command failed: {e}"),
                json!({"argv": meta_argv, "cwd": meta_cwd}),
                false,
            ),
        }
    }

    async fn read_command_output(&self, args: &Value) -> ToolOutcome {
        let requested: Option<String> = args
            .get("command_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let ring = self.command_outputs.lock().unwrap();
        let capture = match &requested {
            Some(id) => ring.iter().rev().find(|c| &c.id == id).cloned(),
            None => ring.back().cloned(),
        };
        let available: Vec<String> = ring.iter().map(|c| c.id.clone()).collect();
        drop(ring);
        let Some(capture) = capture else {
            let hint = if available.is_empty() {
                "no command output yet — run_command first".to_string()
            } else {
                format!(
                    "unknown command_id {:?}; available: {:?}",
                    requested.as_deref().unwrap_or("(most recent)"),
                    available
                )
            };
            return ToolOutcome::fail(hint, json!({}), false);
        };
        let stdout_tail = sandbox_proc::read_capture_tail(&capture.stdout, 16_000);
        let stderr_tail = sandbox_proc::read_capture_tail(&capture.stderr, 8_000);
        ToolOutcome::ok(
            format!(
                "(id: {})\n--- stdout ---\n{}\n--- stderr ---\n{}",
                capture.id,
                stdout_tail,
                stderr_tail.trim_end()
            ),
            json!({"command_id": capture.id}),
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

/// Read up to `cap` bytes from the start of a file (lossy, with a
/// truncation marker when the file is larger). Used by `read_files`.
fn read_head(path: &Path, cap: u64) -> std::io::Result<String> {
    let f = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(cap.min(8192) as usize);
    f.take(cap).read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).to_string();
    if (bytes.len() as u64) >= cap && std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > cap {
        text.push_str("\n[...truncated...]");
    }
    Ok(text)
}

/// Bounded recursive walk of `root`, returning workspace-relative paths in
/// sorted pre-order (deterministic). Skips the hidden `.home`/`.tmp` dirs
/// and stops at `max_entries` entries or `max_depth` levels below root.
fn walk_relative(root: &Path, max_depth: usize, max_entries: usize) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(PathBuf::new(), 0)];
    while let Some((rel, depth)) = stack.pop() {
        let abs = root.join(&rel);
        let Ok(rd) = std::fs::read_dir(&abs) else {
            continue;
        };
        let mut names: Vec<(String, bool)> = Vec::new();
        for entry in rd.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if rel.as_os_str().is_empty() && (name == ".home" || name == ".tmp") {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            names.push((name, is_dir));
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, is_dir) in names.iter().rev() {
            if out.len() >= max_entries {
                return out;
            }
            let child_rel = rel.join(name);
            out.push(child_rel.clone());
            if *is_dir && depth < max_depth {
                stack.push((child_rel, depth + 1));
            }
        }
    }
    out
}

/// Globs match against the searched root; displayed paths are workspace-
/// relative (`root_rel` + `rel`), so the model can paste them directly into
/// read_file/list_directory.
fn display_rel(root_rel: &Path, rel: &Path) -> String {
    if root_rel.as_os_str().is_empty() {
        rel.to_string_lossy().to_string()
    } else {
        root_rel.join(rel).to_string_lossy().to_string()
    }
}

/// Glob match against a relative path: `*` matches any run of characters
/// (including `/`, so `**/x` works naturally), `?` matches exactly one.
/// Case-sensitive; iterative DP so hostile patterns stay linear.
fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = path.chars().collect();
    let (n, m) = (p.len(), s.len());
    let mut dp = vec![vec![false; m + 1]; n + 1];
    dp[0][0] = true;
    for i in 1..=n {
        dp[i][0] = dp[i - 1][0] && p[i - 1] == '*';
    }
    for i in 1..=n {
        for j in 1..=m {
            dp[i][j] = match p[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                c => dp[i - 1][j - 1] && c == s[j - 1],
            };
        }
    }
    dp[n][m]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> (tempfile::TempDir, ToolRuntime) {
        let dir = tempfile::tempdir().unwrap();
        let rt = ToolRuntime::create(
            &dir.path().join("ws"),
            SandboxConfig {
                allowed_commands: vec!["node".into()],
                command_timeout: Duration::from_secs(30),
                read_file_max_bytes: 1024,
                write_file_max_bytes: 1_048_576,
                runtime: crate::runtime::SandboxRuntime::Legacy,
            },
        )
        .unwrap();
        (dir, rt)
    }

    #[tokio::test]
    async fn read_file_paging_skips_lines_before_truncation() {
        let (_dir, rt) = runtime();
        let content = (0..100)
            .map(|i| format!("line {i:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(rt.root().join("big.txt"), &content).unwrap();

        let out = rt
            .execute(
                "read_file",
                &json!({"path": "big.txt", "offset_line": 90, "max_bytes": 50}),
            )
            .await;
        assert!(out.success, "{}", out.summary);
        assert!(out.summary.starts_with("line 090"), "{}", out.summary);
        assert!(!out.summary.contains("line 000"), "{}", out.summary);
        assert!(out.summary.contains("truncated"), "{}", out.summary);
    }

    #[tokio::test]
    async fn read_file_offset_beyond_end_fails() {
        let (_dir, rt) = runtime();
        std::fs::write(rt.root().join("small.txt"), "a\nb\n").unwrap();
        let out = rt
            .execute("read_file", &json!({"path": "small.txt", "offset_line": 2}))
            .await;
        assert!(!out.success);
        assert!(out.summary.contains("beyond end"), "{}", out.summary);
    }

    #[tokio::test]
    async fn read_file_skips_exact_last_line() {
        let (_dir, rt) = runtime();
        std::fs::write(rt.root().join("two.txt"), "a\nb").unwrap();
        let out = rt
            .execute("read_file", &json!({"path": "two.txt", "offset_line": 1}))
            .await;
        assert!(out.success, "{}", out.summary);
        assert_eq!(out.summary, "b");
    }

    #[tokio::test]
    async fn edit_file_rejects_huge_files() {
        let (_dir, rt) = runtime();
        let big = "x".repeat((8 * 1024 * 1024) as usize + 100);
        std::fs::write(rt.root().join("huge.txt"), &big).unwrap();
        let out = rt
            .execute(
                "edit_file",
                &json!({"path": "huge.txt", "old_string": "x", "new_string": "y"}),
            )
            .await;
        assert!(!out.success);
        assert!(out.summary.contains("too large"), "{}", out.summary);
    }

    // ---- new tools ---------------------------------------------------------

    #[tokio::test]
    async fn read_files_batches_and_reports_failures_inline() {
        let (_dir, rt) = runtime();
        std::fs::write(rt.root().join("a.txt"), "alpha").unwrap();
        std::fs::write(rt.root().join("b.txt"), "beta").unwrap();
        let out = rt
            .execute(
                "read_files",
                &json!({"paths": ["a.txt", "missing.txt", "b.txt"]}),
            )
            .await;
        assert!(
            out.success,
            "batch succeeds even with a bad file: {}",
            out.summary
        );
        assert!(out.summary.contains("=== a.txt ==="), "{}", out.summary);
        assert!(out.summary.contains("alpha"), "{}", out.summary);
        assert!(out.summary.contains("ERROR"), "{}", out.summary);
        assert!(out.summary.contains("beta"), "{}", out.summary);
        assert!(
            out.summary.contains("1 file(s) could not be read"),
            "{}",
            out.summary
        );
    }

    #[tokio::test]
    async fn read_files_rejects_too_many_paths() {
        let (_dir, rt) = runtime();
        let paths: Vec<String> = (0..11).map(|i| format!("f{i}.txt")).collect();
        let out = rt.execute("read_files", &json!({"paths": paths})).await;
        assert!(!out.success);
        assert!(out.summary.contains("1 and 10"), "{}", out.summary);
    }

    #[tokio::test]
    async fn append_file_appends_and_ensures_newline() {
        let (_dir, rt) = runtime();
        std::fs::write(rt.root().join("log.txt"), "first").unwrap();
        let out = rt
            .execute(
                "append_file",
                &json!({"path": "log.txt", "content": "second"}),
            )
            .await;
        assert!(out.success, "{}", out.summary);
        let text = std::fs::read_to_string(rt.root().join("log.txt")).unwrap();
        assert_eq!(text, "first\nsecond");
        // Second append: file ends with \n → no extra newline.
        let out = rt
            .execute(
                "append_file",
                &json!({"path": "log.txt", "content": "third"}),
            )
            .await;
        assert!(out.success, "{}", out.summary);
        assert_eq!(
            std::fs::read_to_string(rt.root().join("log.txt")).unwrap(),
            "first\nsecond\nthird"
        );
        // ensure_newline=false appends verbatim.
        let out = rt
            .execute(
                "append_file",
                &json!({"path": "log.txt", "content": "X", "ensure_newline": false}),
            )
            .await;
        assert!(out.success, "{}", out.summary);
        assert_eq!(
            std::fs::read_to_string(rt.root().join("log.txt")).unwrap(),
            "first\nsecond\nthirdX"
        );
    }

    #[tokio::test]
    async fn append_file_enforces_size_cap() {
        let (_dir, rt) = runtime();
        std::fs::write(rt.root().join("big.txt"), "x".repeat(1_048_576)).unwrap();
        let out = rt
            .execute(
                "append_file",
                &json!({"path": "big.txt", "content": "more"}),
            )
            .await;
        assert!(!out.success);
        assert!(out.summary.contains("cap"), "{}", out.summary);
    }

    #[tokio::test]
    async fn move_file_moves_and_creates_parents() {
        let (_dir, rt) = runtime();
        std::fs::write(rt.root().join("old.rs"), "fn main() {}").unwrap();
        let out = rt
            .execute(
                "move_file",
                &json!({"source": "old.rs", "destination": "src/lib/old.rs"}),
            )
            .await;
        assert!(out.success, "{}", out.summary);
        assert!(rt.root().join("src/lib/old.rs").exists());
        assert!(!rt.root().join("old.rs").exists());
    }

    #[tokio::test]
    async fn move_file_rejects_jail_escape() {
        let (_dir, rt) = runtime();
        std::fs::write(rt.root().join("a.txt"), "x").unwrap();
        let out = rt
            .execute(
                "move_file",
                &json!({"source": "a.txt", "destination": "../escape.txt"}),
            )
            .await;
        assert!(!out.success);
        assert!(out.sandbox_violation, "{}", out.summary);
    }

    #[tokio::test]
    async fn copy_file_copies() {
        let (_dir, rt) = runtime();
        std::fs::write(rt.root().join("a.js"), "export const a = 1;").unwrap();
        let out = rt
            .execute(
                "copy_file",
                &json!({"source": "a.js", "destination": "b.js"}),
            )
            .await;
        assert!(out.success, "{}", out.summary);
        assert_eq!(
            std::fs::read_to_string(rt.root().join("b.js")).unwrap(),
            "export const a = 1;"
        );
    }

    #[tokio::test]
    async fn get_file_info_reports_metadata() {
        let (_dir, rt) = runtime();
        std::fs::write(rt.root().join("info.txt"), "hello").unwrap();
        let out = rt
            .execute("get_file_info", &json!({"path": "info.txt"}))
            .await;
        assert!(out.success, "{}", out.summary);
        assert!(out.summary.contains("type: file"), "{}", out.summary);
        assert!(out.summary.contains("size: 5 bytes"), "{}", out.summary);
        let out = rt.execute("get_file_info", &json!({"path": "."})).await;
        assert!(out.success, "{}", out.summary);
        assert!(out.summary.contains("type: directory"), "{}", out.summary);
    }

    #[tokio::test]
    async fn find_files_matches_globs() {
        let (_dir, rt) = runtime();
        std::fs::create_dir_all(rt.root().join("src/tests")).unwrap();
        std::fs::write(rt.root().join("src/app.ts"), "x").unwrap();
        std::fs::write(rt.root().join("src/tests/app.test.ts"), "x").unwrap();
        std::fs::write(rt.root().join("README.md"), "x").unwrap();
        let out = rt
            .execute("find_files", &json!({"pattern": "**/*.test.ts"}))
            .await;
        assert!(out.success, "{}", out.summary);
        assert_eq!(out.summary, "src/tests/app.test.ts", "{}", out.summary);
        let out = rt.execute("find_files", &json!({"pattern": "*.md"})).await;
        assert_eq!(out.summary, "README.md");
    }

    #[tokio::test]
    async fn search_files_finds_substrings_and_honors_case_flag() {
        let (_dir, rt) = runtime();
        std::fs::create_dir_all(rt.root().join("src")).unwrap();
        std::fs::write(
            rt.root().join("src/a.rs"),
            "fn main() {\n    // TODO: fix me\n}\n",
        )
        .unwrap();
        std::fs::write(rt.root().join("src/b.rs"), "let todo = 1;\n").unwrap();
        let out = rt
            .execute("search_files", &json!({"pattern": "TODO", "path": "src"}))
            .await;
        assert!(out.success, "{}", out.summary);
        assert_eq!(
            out.summary, "src/a.rs:2:     // TODO: fix me",
            "{}",
            out.summary
        );
        // Case-insensitive finds the lowercase hit too.
        let out = rt
            .execute(
                "search_files",
                &json!({"pattern": "todo", "path": "src", "case_insensitive": true}),
            )
            .await;
        assert!(out.success, "{}", out.summary);
        assert!(out.summary.contains("src/a.rs:2"), "{}", out.summary);
        assert!(out.summary.contains("src/b.rs:1"), "{}", out.summary);
    }

    #[tokio::test]
    async fn search_files_skips_binary_files() {
        let (_dir, rt) = runtime();
        std::fs::write(rt.root().join("blob.bin"), b"\x00\x01\x02TODO\x00").unwrap();
        std::fs::write(rt.root().join("text.txt"), "TODO here").unwrap();
        let out = rt
            .execute("search_files", &json!({"pattern": "TODO"}))
            .await;
        assert!(out.success, "{}", out.summary);
        assert_eq!(out.summary, "text.txt:1: TODO here", "{}", out.summary);
    }

    #[tokio::test]
    async fn read_workspace_tree_respects_depth_and_caps() {
        let (_dir, rt) = runtime();
        std::fs::create_dir_all(rt.root().join("a/b")).unwrap();
        std::fs::write(rt.root().join("top.txt"), "x").unwrap();
        std::fs::write(rt.root().join("a/deep.txt"), "x").unwrap();
        std::fs::write(rt.root().join("a/b/nested.txt"), "x").unwrap();
        // depth 0: only the top level.
        let out = rt
            .execute("read_workspace_tree", &json!({"depth": 0}))
            .await;
        assert!(out.success, "{}", out.summary);
        let lines: Vec<&str> = out.summary.lines().collect();
        assert!(lines.contains(&"top.txt (1 bytes)"), "{}", out.summary);
        assert!(lines.contains(&"a/"), "{}", out.summary);
        assert!(!out.summary.contains("deep.txt"), "{}", out.summary);
        // depth 2 reaches nested files.
        let out = rt
            .execute("read_workspace_tree", &json!({"depth": 2}))
            .await;
        assert!(out.success, "{}", out.summary);
        assert!(out.summary.contains("nested.txt"), "{}", out.summary);
    }

    #[tokio::test]
    async fn run_command_cwd_changes_working_directory() {
        let (_dir, rt) = runtime();
        std::fs::create_dir_all(rt.root().join("sub")).unwrap();
        let out = rt
            .execute(
                "run_command",
                &json!({"argv": ["node", "-e", "console.log(process.cwd())"], "cwd": "sub"}),
            )
            .await;
        assert!(out.success, "{}", out.summary);
        assert!(out.summary.contains("sub"), "{}", out.summary);
        // cwd must exist.
        let out = rt
            .execute(
                "run_command",
                &json!({"argv": ["node", "-e", "console.log('x')"], "cwd": "nope"}),
            )
            .await;
        assert!(!out.success);
        assert!(out.summary.contains("not a directory"), "{}", out.summary);
    }

    #[tokio::test]
    async fn read_command_output_fetches_by_id() {
        let (_dir, rt) = runtime();
        let first = rt
            .execute(
                "run_command",
                &json!({"argv": ["node", "-e", "console.log('first')"]}),
            )
            .await;
        assert!(first.success, "{}", first.summary);
        let id = first.metadata["command_id"].as_str().unwrap().to_string();
        let second = rt
            .execute(
                "run_command",
                &json!({"argv": ["node", "-e", "console.log('second')"]}),
            )
            .await;
        assert!(second.success, "{}", second.summary);
        // Most recent by default.
        let out = rt.execute("read_command_output", &json!({})).await;
        assert!(out.success, "{}", out.summary);
        assert!(out.summary.contains("second"), "{}", out.summary);
        // Explicit id fetches the older one.
        let out = rt
            .execute("read_command_output", &json!({"command_id": id}))
            .await;
        assert!(out.success, "{}", out.summary);
        assert!(out.summary.contains("first"), "{}", out.summary);
        // Unknown id is a clear error.
        let out = rt
            .execute("read_command_output", &json!({"command_id": "cmd-nope"}))
            .await;
        assert!(!out.success);
        assert!(out.summary.contains("available"), "{}", out.summary);
    }
}
