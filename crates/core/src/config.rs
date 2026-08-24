use serde::{Deserialize, Serialize};
use std::path::Path;

/// Runner-wide configuration, loaded from `~/.config/lmhub/config.toml`
/// (all fields optional; defaults below are used for anything missing).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct AppConfig {
    /// Prompt filename (without extension) preselected in the TUI.
    pub default_prompt: Option<String>,
    /// Task prompt filename (without extension) preselected in the TUI.
    pub default_task_prompt: Option<String>,
    /// Hard wall-clock deadline for a whole run.
    pub run_timeout_secs: u64,
    /// Maximum agent turns (LLM requests) per run.
    pub max_turns: u32,
    /// Per-command timeout for the `run_command` tool.
    pub command_timeout_secs: u64,
    /// Allowlist for `run_command` argv[0]. Only exact names pass.
    pub allowed_commands: Vec<String>,
    /// How long the Models.dev cache stays fresh.
    pub modelsdev_ttl_secs: u64,
    /// max_tokens sent to the provider.
    pub max_output_tokens: u32,
    /// Sandbox read_file cap (bytes).
    pub read_file_max_bytes: u64,
    /// Sandbox write_file cap (bytes).
    pub write_file_max_bytes: u64,
    /// Attempts per HTTP request against providers (incl. the first one).
    pub max_retries: u32,
    /// Exponential backoff base delay.
    pub retry_base_ms: u64,
    /// Backoff cap.
    pub retry_cap_ms: u64,
    /// Command isolation backend: `auto` (bubblewrap when available, legacy
    /// otherwise), `bwrap` (require it), or `legacy` (no OS isolation).
    pub sandbox: String,
}

/// Default command allowlist: common developer tooling a coding agent needs
/// in the workspace jail. Everything here runs via argv arrays (no shell);
/// a missing binary fails per-command, never silently.
pub const DEFAULT_ALLOWED_COMMANDS: &[&str] = &[
    "awk", "cat", "chmod", "cp", "curl", "cut", "diff", "echo", "find", "git", "grep", "head",
    "jq", "ls", "make", "mkdir", "mv", "node", "npm", "npx", "pnpm", "pwd", "python3", "rm", "sed",
    "sort", "tail", "tar", "tee", "touch", "uniq", "wc", "wget", "which", "xargs",
];

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_prompt: None,
            default_task_prompt: None,
            run_timeout_secs: 900,
            max_turns: 30,
            command_timeout_secs: 90,
            allowed_commands: DEFAULT_ALLOWED_COMMANDS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            modelsdev_ttl_secs: 86_400,
            max_output_tokens: 16_384,
            read_file_max_bytes: 48_000,
            write_file_max_bytes: 1_000_000,
            max_retries: 6,
            retry_base_ms: 500,
            retry_cap_ms: 30_000,
            sandbox: "auto".into(),
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        toml::from_str(&raw).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Clamp zero/absurd values to safe minimums so a typo in config.toml
    /// (e.g. `run_timeout_secs = 0`) cannot silently produce instantly
    /// failing runs or a retry storm. Warns about every correction.
    pub fn sanitize(&mut self) {
        fn fix<T>(value: &mut T, min: T, name: &str)
        where
            T: PartialOrd + Copy + std::fmt::Display + std::fmt::Debug,
        {
            if *value < min {
                tracing::warn!(config = name, from = %*value, to = %min, "config value clamped to minimum");
                *value = min;
            }
        }
        fix(&mut self.run_timeout_secs, 1, "run_timeout_secs");
        fix(&mut self.max_turns, 1, "max_turns");
        fix(&mut self.command_timeout_secs, 1, "command_timeout_secs");
        fix(&mut self.modelsdev_ttl_secs, 1, "modelsdev_ttl_secs");
        fix(&mut self.max_output_tokens, 1, "max_output_tokens");
        fix(&mut self.read_file_max_bytes, 1, "read_file_max_bytes");
        fix(&mut self.write_file_max_bytes, 1, "write_file_max_bytes");
        fix(&mut self.max_retries, 1, "max_retries");
        fix(&mut self.retry_base_ms, 1, "retry_base_ms");
        if self.retry_cap_ms < self.retry_base_ms {
            tracing::warn!(
                config = "retry_cap_ms",
                from = self.retry_cap_ms,
                to = self.retry_base_ms,
                "retry_cap_ms below retry_base_ms; clamped"
            );
            self.retry_cap_ms = self.retry_base_ms;
        }
        // An empty allowlist means no command can ever run; treat it as a
        // config mistake and restore the safe defaults.
        let cleaned: Vec<String> = self
            .allowed_commands
            .iter()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect();
        if cleaned.is_empty() && !self.allowed_commands.is_empty() {
            tracing::warn!("allowed_commands empty or all-blank; restoring defaults");
            self.allowed_commands = DEFAULT_ALLOWED_COMMANDS
                .iter()
                .map(|s| s.to_string())
                .collect();
        } else {
            self.allowed_commands = cleaned;
        }
        let sb = self.sandbox.trim().to_ascii_lowercase();
        if sb != "auto" && sb != "bwrap" && sb != "legacy" {
            tracing::warn!(
                config = "sandbox",
                value = %self.sandbox,
                "unknown sandbox mode; falling back to auto"
            );
            self.sandbox = "auto".into();
        } else {
            self.sandbox = sb;
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            path,
            toml::to_string_pretty(self).expect("serialize config"),
        )
    }

    pub fn run_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.run_timeout_secs)
    }

    pub fn command_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.command_timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_clamps_zero_values_and_retry_cap() {
        let mut cfg = AppConfig {
            run_timeout_secs: 0,
            max_turns: 0,
            command_timeout_secs: 0,
            max_retries: 0,
            retry_base_ms: 0,
            retry_cap_ms: 0,
            ..AppConfig::default()
        };
        cfg.sanitize();
        assert!(cfg.run_timeout_secs >= 1);
        assert!(cfg.max_turns >= 1);
        assert!(cfg.command_timeout_secs >= 1);
        assert!(cfg.max_retries >= 1);
        assert!(cfg.retry_base_ms >= 1);
        assert!(cfg.retry_cap_ms >= cfg.retry_base_ms);
    }

    #[test]
    fn sanitize_restores_empty_allowlist() {
        let mut cfg = AppConfig {
            allowed_commands: vec!["  ".into(), "".into()],
            ..AppConfig::default()
        };
        cfg.sanitize();
        assert_eq!(
            cfg.allowed_commands,
            DEFAULT_ALLOWED_COMMANDS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }
}
