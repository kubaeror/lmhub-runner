//! lmhub-runner — independent AI runner: models as sandboxed coding agents.
//!
//! Launch with `cargo run` (no CLI flags): the interactive TUI handles
//! provider/model/reasoning/prompt selection, live runs and history.

use anyhow::Context;
use lmhub_core::{AppConfig, DEFAULT_SYSTEM_PROMPT};
use lmhub_modelsdev::ModelsDevClient;
use lmhub_tui::{PromptFile, TuiContext};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join(".config"))
        .join("lmhub");

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
    let output_base = project_dir.join("output");
    let providers_dir = project_dir.join("providers");

    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(|| project_dir.join(".cache"))
        .join("lmhub");
    let config_path = config_dir.join("config.toml");
    let config = AppConfig::load(&config_path).unwrap_or_default();

    lmhub_providers::init_retry_policy(lmhub_providers::http::RetryPolicy {
        max_attempts: config.max_retries.max(1),
        base: Duration::from_millis(config.retry_base_ms),
        cap: Duration::from_millis(config.retry_cap_ms),
    });

    init_logging(&cache_dir)?;

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

    let prompts = discover_prompts(&[
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
        output_base,
        auth_store,
    };
    lmhub_tui::run_tui(ctx).await
}

/// Structured logs go to a file — the terminal belongs to the TUI.
fn init_logging(cache_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(cache_dir)?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cache_dir.join("runner.log"))?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hyper=warn,reqwest=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();
    Ok(())
}

/// Discover prompt files (`*.md`) across directories, later dirs deduped.
/// Guarantees at least one usable prompt: writes `default.md` on first run.
fn discover_prompts(dirs: &[PathBuf]) -> Vec<PromptFile> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
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
                out.push(PromptFile {
                    name: name.to_string(),
                    path: path.clone(),
                });
            }
        }
    }

    if out.is_empty() {
        // First-run convenience: materialize the built-in prompt as a file
        // users can edit or duplicate.
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
                return vec![PromptFile {
                    name: "default".into(),
                    path: target,
                }];
            }
        }
        // Absolute fallback: virtual entry backed by the embedded constant.
        out.push(PromptFile {
            name: "built-in".into(),
            path: PathBuf::new(),
        });
    }
    out
}
