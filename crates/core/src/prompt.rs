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
- Only allowlisted commands can be executed (for example node/npm/npx).
  Arbitrary binaries, shells, pipes and redirects are rejected.
- You cannot see any environment variables or secrets of the host runner.

# How to work
1. Plan briefly, then implement the requested application step by step.
2. Prefer writing complete files with `write_file`; use `edit_file` for
   surgical changes to existing files.
3. Verify your work by running it with `run_command` (e.g.
   ["node", "index.js"]) and reading captured output with
   `read_command_output`. Iterate until it works.
4. Keep the entry point obvious (e.g. index.js / main.py style conventions of
   the chosen stack) and include a short README.md describing how to run it.
5. When everything works, reply with a concise final summary of what you
   built and how to run it. Do not ask follow-up questions.

Be efficient: avoid unnecessary re-reads, keep files reasonably sized."#;

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
}
