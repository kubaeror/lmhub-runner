//! lmhub-runner — independent AI runner: models as sandboxed coding agents.
//!
//! Launch with `cargo run` (no CLI flags): the interactive TUI handles
//! provider/model/reasoning/prompt selection, live runs and history.

use anyhow::Context;
use lmhub_core::{AppConfig, DEFAULT_SYSTEM_PROMPT, DEFAULT_TASK_PROMPT};
use lmhub_modelsdev::ModelsDevClient;
use lmhub_tui::{PromptFile, TuiContext};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let config_dir = std::env::var_os("LMHUB_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join(".config"))
                .join("lmhub")
        });

    // Load stored credentials and register them for scrubbing BEFORE the
    // redaction engine initializes, so they can never reach logs/stats.
    let auth_store = Arc::new(std::sync::Mutex::new(lmhub_core::AuthStore::load(
        lmhub_core::AuthStore::path_for(&config_dir),
    )));
    for secret in auth_store.lock().unwrap().all_secrets() {
        lmhub_core::redact::register_extra(&secret);
    }
    // Collect runner env secrets too (keys/tokens), then freeze the list.
    lmhub_core::redact::init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    runtime.block_on(run(config_dir, auth_store))
}

async fn run(
    config_dir: PathBuf,
    auth_store: Arc<std::sync::Mutex<lmhub_core::AuthStore>>,
) -> anyhow::Result<()> {
    let project_dir = std::env::current_dir().context("current directory")?;
    let output_base = std::env::var_os("LMHUB_OUTPUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| project_dir.join("output"));
    let providers_dir = std::env::var_os("LMHUB_PROVIDERS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| project_dir.join("providers"));

    let cache_dir = std::env::var_os("LMHUB_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::cache_dir()
                .unwrap_or_else(|| project_dir.join(".cache"))
                .join("lmhub")
        });
    let config_path = config_dir.join("config.toml");
    // A missing config.toml is normal (defaults); a *broken* one is a
    // user error we refuse to paper over with weaker defaults.
    let mut config = match AppConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("lmhub: no {} — using defaults", config_path.display());
            AppConfig::default()
        }
        Err(e) => {
            anyhow::bail!(
                "{} is invalid: {e} — fix it or delete it to start with defaults",
                config_path.display()
            )
        }
    };
    config.sanitize();

    lmhub_providers::init_retry_policy(lmhub_providers::http::RetryPolicy {
        max_attempts: config.max_retries.max(1),
        base: Duration::from_millis(config.retry_base_ms),
        cap: Duration::from_millis(config.retry_cap_ms),
    });

    init_logging(&cache_dir)?;

    // Resolve the command-isolation backend once; warn loudly on fallback
    // (after logging is live so the warning also lands in runner.log).
    let (sandbox_runtime, sandbox_warnings) = lmhub_sandbox::detect_runtime(&config.sandbox);
    for w in &sandbox_warnings {
        tracing::warn!("{w}");
        eprintln!("lmhub: {w}");
    }

    lmhub_modelsdev::ensure_cache_dir(&cache_dir)
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;
    let modelsdev = Arc::new(ModelsDevClient::new(
        cache_dir.clone(),
        Duration::from_secs(config.modelsdev_ttl_secs),
    ));

    let (registry, provider_errors) =
        lmhub_providers::build_registry(&providers_dir, Arc::clone(&auth_store));
    for err in &provider_errors {
        tracing::warn!("custom provider skipped: {err}");
        eprintln!("lmhub: custom provider skipped: {err}");
    }

    let (prompts, task_prompts) = discover_prompt_lists(&[
        project_dir.join("prompts"),
        config_dir.join("prompts"),
        cache_dir.join("prompts"),
    ]);

    let ctx = TuiContext {
        registry,
        modelsdev,
        config,
        config_path,
        prompts,
        task_prompts,
        output_base,
        auth_store,
        sandbox_runtime,
    };
    lmhub_tui::run_tui(ctx).await
}

/// Structured logs go to a file — the terminal belongs to the TUI.
///
/// The file is size-capped (truncated once past 10 MiB on startup) and every
/// line is scrubbed of registered secrets before it is written, so
/// `runner.log` never contains API keys/tokens even if a log call bypasses
/// the redaction-aware sinks.
fn init_logging(cache_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(cache_dir)?;
    let log_path = cache_dir.join("runner.log");
    const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
    if std::fs::metadata(&log_path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)?;
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hyper=warn,reqwest=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(ScrubbingMakeWriter {
            file: Arc::new(Mutex::new(log_file)),
        })
        .with_ansi(false)
        .init();
    Ok(())
}

/// Writer factory for the log file; scrubs secrets line-by-line.
#[derive(Clone)]
struct ScrubbingMakeWriter {
    file: Arc<Mutex<std::fs::File>>,
}

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for ScrubbingMakeWriter {
    type Writer = ScrubbingFileWriter;
    fn make_writer(&'a self) -> Self::Writer {
        ScrubbingFileWriter {
            file: Arc::clone(&self.file),
            pending: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// Line-buffering file writer that passes complete lines through
/// `redact::scrub` before writing them to disk.
struct ScrubbingFileWriter {
    file: Arc<Mutex<std::fs::File>>,
    pending: Arc<Mutex<Vec<u8>>>,
}

impl std::io::Write for ScrubbingFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.extend_from_slice(buf);
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            let scrubbed = lmhub_core::redact::scrub(&String::from_utf8_lossy(&line));
            self.file
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .write_all(scrubbed.as_bytes())?;
        }
        // A single gigantic unterminated line must not grow memory without
        // bound; flush it scrubbed as-is.
        if pending.len() > 64 * 1024 {
            let scrubbed = lmhub_core::redact::scrub(&String::from_utf8_lossy(&pending));
            self.file
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .write_all(scrubbed.as_bytes())?;
            pending.clear();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

/// Discover prompt files (`*.md`) across directories, later dirs deduped.
///
/// Two independent lists, mirroring the TUI's two pickers:
/// - **system prompts**: files at the root of each `prompts/` dir (backward
///   compatible with the old layout) plus `prompts/system-prompts/`;
/// - **task prompts**: files in `prompts/task-prompts/` — the user
///   instruction sent as the first message, replacing the old free-text task.
///
/// Guarantees both lists are non-empty: writes a `default.md` on first run
/// (system) and a `task-prompts/default.md` (task), with virtual built-in
/// entries as the absolute fallback.
fn discover_prompt_lists(dirs: &[PathBuf]) -> (Vec<PromptFile>, Vec<PromptFile>) {
    const SYSTEM_SUBDIR: &str = "system-prompts";
    const TASK_SUBDIR: &str = "task-prompts";
    let mut seen_system = std::collections::BTreeSet::new();
    let mut seen_task = std::collections::BTreeSet::new();
    let mut system = Vec::new();
    let mut task = Vec::new();

    for dir in dirs {
        scan_prompt_dir(dir, None, &mut system, &mut seen_system);
        scan_prompt_dir(dir, Some(SYSTEM_SUBDIR), &mut system, &mut seen_system);
        scan_prompt_dir(dir, Some(TASK_SUBDIR), &mut task, &mut seen_task);
    }

    if system.is_empty() {
        // First-run convenience: materialize the built-in system prompt as a
        // file users can edit or duplicate.
        if let Some(first) = dirs.first() {
            if std::fs::create_dir_all(first).is_ok() {
                let target = first.join("default.md");
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&target)
                {
                    let _ = f.write_all(DEFAULT_SYSTEM_PROMPT.as_bytes());
                }
                if target.exists() {
                    system.push(PromptFile {
                        name: "default".into(),
                        path: target,
                    });
                }
            }
        }
        if system.is_empty() {
            // Absolute fallback: virtual entry backed by the embedded constant.
            system.push(PromptFile {
                name: "built-in".into(),
                path: PathBuf::new(),
            });
        }
    }

    if task.is_empty() {
        if let Some(first) = dirs.first() {
            let target = first.join(TASK_SUBDIR).join("default.md");
            if std::fs::create_dir_all(target.parent().expect("subdir has parent")).is_ok() {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&target)
                {
                    let _ = f.write_all(DEFAULT_TASK_PROMPT.as_bytes());
                }
                if target.exists() {
                    task.push(PromptFile {
                        name: "default".into(),
                        path: target,
                    });
                }
            }
        }
        if task.is_empty() {
            task.push(PromptFile {
                name: "built-in".into(),
                path: PathBuf::new(),
            });
        }
    }
    (system, task)
}

/// Scan one prompt directory (root or a `system-prompts`/`task-prompts`
/// subdir), appending new `*.md` files to `list` (deduped by stem name).
fn scan_prompt_dir(
    dir: &Path,
    subdir: Option<&str>,
    list: &mut Vec<PromptFile>,
    seen: &mut std::collections::BTreeSet<String>,
) {
    let scan_dir = match subdir {
        Some(s) => dir.join(s),
        None => dir.to_path_buf(),
    };
    let Ok(entries) = std::fs::read_dir(&scan_dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
        .collect();
    files.sort();
    for path in files {
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if seen.insert(name.to_string()) {
            list.push(PromptFile {
                name: name.to_string(),
                path: path.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_discovery_splits_root_system_and_task_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = dir.path().join("prompts");
        std::fs::create_dir_all(prompts.join("system-prompts")).unwrap();
        std::fs::create_dir_all(prompts.join("task-prompts")).unwrap();
        std::fs::write(prompts.join("legacy.md"), "sys").unwrap();
        std::fs::write(prompts.join("system-prompts").join("deep.md"), "sys").unwrap();
        std::fs::write(prompts.join("task-prompts").join("build.md"), "task").unwrap();
        std::fs::write(prompts.join("task-prompts").join("refactor.md"), "task").unwrap();

        let (system, task) = discover_prompt_lists(&[prompts]);
        let names: Vec<&str> = system.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["legacy", "deep"], "root files are system prompts");
        let names: Vec<&str> = task.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["build", "refactor"]);
    }

    #[test]
    fn prompt_discovery_materializes_defaults_on_first_run() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = dir.path().join("prompts");
        let (system, task) = discover_prompt_lists(std::slice::from_ref(&prompts));
        assert_eq!(system.len(), 1);
        assert_eq!(task.len(), 1);
        assert_eq!(system[0].name, "default");
        assert_eq!(task[0].name, "default");
        assert!(prompts.join("default.md").exists());
        assert!(prompts.join("task-prompts").join("default.md").exists());
    }
}
