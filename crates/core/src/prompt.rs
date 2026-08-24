//! Public system prompt handling: configurable markdown files with a
//! built-in fallback so a missing/broken file never breaks a run.

use std::path::Path;

pub const DEFAULT_SYSTEM_PROMPT: &str = r#"You are an autonomous coding agent working inside your own private workspace directory.

# Workspace rules
- Your workspace is the ONLY place where you may read or write files.
  Paths you pass are interpreted relative to the workspace root; absolute
  paths are re-based onto the workspace automatically.
- Attempting to escape the workspace (e.g. `..`, symlinks outside) is blocked
  and logged. Never try it.
- Only allowlisted commands can be executed — the exact list is in the
  "Available commands" section appended to this prompt. Arbitrary binaries,
  shells, pipes and redirects are rejected.
- You cannot see any environment variables or secrets of the host runner.

# How to work
1. Plan briefly, then implement the requested application step by step.
2. Orient with `read_workspace_tree`/`find_files`/`search_files`; prefer
   writing complete files with `write_file`; use `edit_file` for
   surgical changes to existing files and `append_file` for incremental
   content.
3. Verify your work by running it with `run_command` (e.g.
   ["node", "index.js"]) and reading captured output with
   `read_command_output` (omit command_id for the most recent). Iterate
   until it works.
4. Keep the entry point obvious (e.g. index.js / main.py style conventions of
   the chosen stack) and include a short README.md describing how to run it.
5. When everything works, reply with a concise final summary of what you
   built and how to run it. Do not ask follow-up questions.

Be efficient: avoid unnecessary re-reads, keep files reasonably sized."#;

/// Append the *effective* command allowlist and workspace path rules to a
/// system prompt. Prompt files are static and config-agnostic, so they can
/// only say "some commands are allowed"; the real list lives in `AppConfig`.
/// Injecting it right before the request is sent guarantees the model sees
/// exactly what the sandbox will accept — and explicitly what it will
/// reject (`pwd`-style orientation commands are in the list, host absolute
/// paths never resolve inside the jail). Empty allowlists append nothing;
/// the prompt is returned untouched.
pub fn augment_system_prompt(system_prompt: &str, allowed_commands: &[String]) -> String {
    if allowed_commands.is_empty() {
        return system_prompt.to_string();
    }
    let mut out = String::with_capacity(system_prompt.len() + 512);
    out.push_str(system_prompt);
    out.push_str("\n\n## Available commands\n\n");
    out.push_str(
        "`run_command` accepts exactly these programs (bare argv[0] match, no shell, no pipes, \
         no redirects):\n\n",
    );
    for cmd in allowed_commands {
        out.push_str(&format!("- `{cmd}`\n"));
    }
    out.push_str("\nPath rules (strict):\n");
    out.push_str(
        "- All paths are relative to the workspace root; `.home` is your sandboxed HOME.\n",
    );
    out.push_str(
        "- Absolute paths are re-based onto the workspace. Host paths such as `/home/...`, \
         `/tmp`, `/usr` never exist inside the sandbox — using them yields `No such file or \
         directory`.\n",
    );
    out.push_str(
        "- `run_command` requires `argv`: a non-empty JSON array of strings, e.g. \
         `[\"node\", \"--version\"]`. A call without `argv` is rejected.\n",
    );
    out
}

/// Load a prompt file; fall back to the built-in prompt on any failure
/// (missing file, unreadable, invalid UTF-8).
pub fn load_prompt(path: &Path) -> String {
    load_prompt_file(path, DEFAULT_SYSTEM_PROMPT, "prompt")
}

/// Default user instruction (the first user message) when no task prompt
/// file is configured — a deliberately generic build request so the agent
/// system prompt's "implement the requested application" rule still applies.
pub const DEFAULT_TASK_PROMPT: &str = r#"Build a small application from scratch inside the workspace: pick an appropriate stack, implement it with complete files, verify it runs, and finish with a concise summary of what you built and how to run it."#;

/// Load a task prompt file; falls back to [`DEFAULT_TASK_PROMPT`] on any
/// failure (missing file, unreadable, invalid UTF-8) — never to the system
/// prompt, since that would silently change the agent's role.
pub fn load_task_prompt(path: &Path) -> String {
    load_prompt_file(path, DEFAULT_TASK_PROMPT, "task prompt")
}

fn load_prompt_file(path: &Path, fallback: &str, what: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_e| {
        eprintln!(
            "lmhub: {what} file {} unreadable ({}); using built-in default",
            path.display(),
            _e
        );
        fallback.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_on_missing_file() {
        let p = load_prompt(Path::new("/nonexistent/prompt.md"));
        assert_eq!(p, DEFAULT_SYSTEM_PROMPT);
    }

    #[test]
    fn task_prompt_falls_back_to_task_default() {
        let p = load_task_prompt(Path::new("/nonexistent/task.md"));
        assert_eq!(p, DEFAULT_TASK_PROMPT);
        assert_ne!(p, DEFAULT_SYSTEM_PROMPT);
    }

    #[test]
    fn augment_appends_allowlist_and_rules() {
        let base = "you are an agent.";
        let cmds = ["node".to_string(), "pwd".to_string()];
        let out = augment_system_prompt(base, &cmds);
        assert!(out.starts_with(base));
        assert!(out.contains("`node`"));
        assert!(out.contains("`pwd`"));
        assert!(out.contains("re-based onto the workspace"));
        assert!(out.contains("requires `argv`"));
    }

    #[test]
    fn augment_empty_allowlist_is_identity() {
        let base = "you are an agent.";
        assert_eq!(augment_system_prompt(base, &[]), base);
    }
}
