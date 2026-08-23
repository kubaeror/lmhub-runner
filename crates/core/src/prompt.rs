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
    std::fs::read_to_string(path).unwrap_or_else(|_e| {
        eprintln!(
            "lmhub: prompt file {} unreadable ({}); using built-in default",
            path.display(),
            _e
        );
        DEFAULT_SYSTEM_PROMPT.to_string()
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
}
