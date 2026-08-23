//! Application state + event handling for the TUI.

use crate::{PromptFile, UiMsg};
use lmhub_core::{AppConfig, ModelInfo, ReasoningLevel, RunEvent, StoredCredential, Usage};
use lmhub_modelsdev::CatalogSnapshot;
use lmhub_providers::ProviderRegistry;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Modal overlays on top of the Setup tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Typing an API key for `provider_id`.
    EnterKey {
        provider_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Setup,
    Run,
    History,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Setup, Tab::Run, Tab::History];
    pub fn title(&self) -> &'static str {
        match self {
            Tab::Setup => "[1] Setup",
            Tab::Run => "[2] Run",
            Tab::History => "[3] History",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Providers,
    Models,
    Reasoning,
    Prompts,
    Task,
}

#[derive(Debug, Clone)]
pub struct HistoryRow {
    pub path: PathBuf,
    pub family: String,
    pub model: String,
    pub reasoning: String,
    pub status: String,
    pub duration_ms: Option<u64>,
    pub total_tokens: Option<u64>,
    pub total_usd: Option<f64>,
}

pub struct ActiveRun {
    pub provider_id: String,
    pub model_id: String,
    pub reasoning: String,
    pub started: Instant,
    pub cancel: CancellationToken,
    pub feed_lines: Vec<String>,
    pub tokens: Usage,
    pub tool_ok: u64,
    pub tool_fail: u64,
    pub errors: u32,
    pub warnings: u32,
    pub pricing: Option<lmhub_core::ModelPricing>,
    pub finished_line: Option<String>,
    pub scroll: usize,
    /// Live text of the current streamed turn (tail-capped).
    pub live_turn: String,
    /// Number of deltas received this run (activity indicator).
    pub delta_count: u64,
}

const LIVE_TURN_CAP_CHARS: usize = 2_000;

fn append_live_tail(live: &mut String, text: &str) {
    live.push_str(text);
    let total = live.chars().count();
    if total > LIVE_TURN_CAP_CHARS {
        let skip = total - LIVE_TURN_CAP_CHARS;
        *live = live.chars().skip(skip).collect();
    }
}

pub struct App {
    pub registry: ProviderRegistry,
    pub modelsdev: Arc<lmhub_modelsdev::ModelsDevClient>,
    pub config: AppConfig,
    pub config_path: PathBuf,
    pub prompts: Vec<PromptFile>,
    pub output_base: PathBuf,
    pub auth_store: std::sync::Arc<std::sync::Mutex<lmhub_core::AuthStore>>,
    ui_tx: tokio::sync::mpsc::UnboundedSender<UiMsg>,

    pub mode: Mode,
    pub key_input: String,
    pub provider_filter: String,
    /// Transient status message (auto-hidden).
    pub notice: Option<(String, Instant)>,

    pub tab: Tab,
    pub should_quit: bool,

    // setup state
    pub focus: Focus,
    pub provider_idx: usize,
    pub models: Vec<ModelInfo>,
    pub models_loading: bool,
    pub model_source: Option<&'static str>,
    pub model_warnings: Vec<String>,
    pub model_idx: usize,
    pub snapshot: Option<Arc<CatalogSnapshot>>,
    pub reasoning_idx: usize, // index into visible levels
    pub prompt_idx: usize,
    pub task_input: String,

    pub run: Option<ActiveRun>,

    // history state
    pub history: Vec<HistoryRow>,
    pub history_idx: usize,
    pub history_detail: Option<String>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: ProviderRegistry,
        modelsdev: Arc<lmhub_modelsdev::ModelsDevClient>,
        config: AppConfig,
        config_path: PathBuf,
        prompts: Vec<PromptFile>,
        output_base: PathBuf,
        auth_store: std::sync::Arc<std::sync::Mutex<lmhub_core::AuthStore>>,
        ui_tx: tokio::sync::mpsc::UnboundedSender<UiMsg>,
    ) -> Self {
        // Preselect default prompt if configured.
        let prompt_idx = config
            .default_prompt
            .as_ref()
            .and_then(|name| prompts.iter().position(|p| &p.name == name))
            .unwrap_or(0);
        Self {
            registry,
            modelsdev,
            config,
            config_path,
            prompts,
            output_base,
            auth_store,
            ui_tx,
            mode: Mode::Normal,
            key_input: String::new(),
            provider_filter: String::new(),
            notice: None,
            tab: Tab::Setup,
            should_quit: false,
            focus: Focus::Providers,
            provider_idx: 0,
            models: Vec::new(),
            models_loading: false,
            model_source: None,
            model_warnings: Vec::new(),
            model_idx: 0,
            snapshot: None,
            reasoning_idx: 0,
            prompt_idx,
            task_input: String::new(),
            run: None,
            history: Vec::new(),
            history_idx: 0,
            history_detail: None,
        }
    }

    pub fn selected_provider(&self) -> Option<Arc<dyn lmhub_core::Provider>> {
        self.filtered_indices()
            .get(self.provider_idx)
            .and_then(|i| self.registry.all().get(*i))
            .cloned()
    }

    /// Indices into registry.all() after applying the text filter.
    pub fn filtered_indices(&self) -> Vec<usize> {
        let f = self.provider_filter.to_ascii_lowercase();
        self.registry
            .all()
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                if f.is_empty() {
                    true
                } else {
                    p.display_name().to_ascii_lowercase().contains(&f)
                        || p.id().to_ascii_lowercase().contains(&f)
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Credential badge for a provider row.
    pub fn provider_badge(&self, registry_idx: usize) -> &'static str {
        let Some(p) = self.registry.all().get(registry_idx) else {
            return "";
        };
        if !p.requires_credentials() {
            return "[local]";
        }
        let stored = {
            let store = self.auth_store.lock().unwrap();
            store.get(p.id()).is_some()
        };
        if stored {
            return "[key ok]";
        }
        for env_key in p.env_keys() {
            if std::env::var(env_key)
                .map(|v| v.len() >= 8)
                .unwrap_or(false)
            {
                return "[key ok]";
            }
        }
        "[no key]"
    }

    pub fn push_notice(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), Instant::now()));
    }

    /// Open connect flow for the provider under the cursor.
    pub fn start_connect(&mut self) {
        let Some(provider) = self.selected_provider() else {
            return;
        };
        let id = provider.id().to_string();
        if id == "github-copilot" {
            self.begin_copilot_flow();
        } else {
            self.mode = Mode::EnterKey { provider_id: id };
            self.key_input.clear();
        }
    }

    fn begin_copilot_flow(&mut self) {
        let tx = self.ui_tx.clone();
        let auth_store = std::sync::Arc::clone(&self.auth_store);
        self.push_notice("copilot: starting device flow…");
        tokio::spawn(async move {
            let result = lmhub_providers::copilot::run_full_flow(&auth_store, |line| {
                let _ = tx.send(UiMsg::Notice(line));
            })
            .await;
            if let Err(e) = result {
                let _ = tx.send(UiMsg::Notice(format!("✖ copilot: {e}")));
            }
        });
    }

    pub fn save_entered_key(&mut self) -> bool {
        let Mode::EnterKey { provider_id } = &self.mode else {
            return false;
        };
        let id = provider_id.clone();
        let key = self.key_input.trim().to_string();
        if key.len() < 8 {
            self.push_notice("key too short");
            return false;
        }
        let save_result = {
            let mut store = self.auth_store.lock().unwrap();
            store.set_credential(&id, StoredCredential::api(key));
            store.save()
        };
        match save_result {
            Ok(()) => {
                self.mode = Mode::Normal;
                self.key_input.clear();
                self.push_notice(format!("saved key for {id}"));
                true
            }
            Err(e) => {
                self.push_notice(format!("✖ saving auth.json failed: {e}"));
                false
            }
        }
    }

    pub fn selected_model(&self) -> Option<&ModelInfo> {
        self.models.get(self.model_idx)
    }

    pub fn visible_reasoning_levels(&self) -> Vec<ReasoningLevel> {
        match self.selected_model() {
            Some(m) if m.capabilities.reasoning => ReasoningLevel::ALL.to_vec(),
            Some(_) => vec![ReasoningLevel::Off],
            None => vec![ReasoningLevel::Off],
        }
    }

    pub fn selected_reasoning(&self) -> ReasoningLevel {
        let levels = self.visible_reasoning_levels();
        levels
            .get(self.reasoning_idx.min(levels.len().saturating_sub(1)))
            .copied()
            .unwrap_or(ReasoningLevel::Off)
    }

    pub fn selected_pricing(&self) -> Option<lmhub_core::PricingContext> {
        let snapshot = self.snapshot.as_ref()?;
        let provider = self.selected_provider()?;
        let hint = provider.models_dev_hint();
        let model = self.selected_model()?;
        lmhub_providers::pricing_context_in_snapshot(snapshot, hint, &model.id)
    }

    /// Kick off async resolution of the model list for the current provider.
    pub fn request_models(&mut self) {
        let Some(provider) = self.selected_provider() else {
            return;
        };
        if self.models_loading {
            return; // already in flight
        }
        self.models_loading = true;
        self.models.clear();
        self.model_idx = 0;
        self.snapshot = None;
        let mdc = Arc::clone(&self.modelsdev);
        let tx = self.ui_tx.clone();
        let requested_for = provider.id().to_string();
        tokio::spawn(async move {
            let catalog = lmhub_providers::resolve_model_catalog(provider.as_ref(), &mdc).await;
            let snapshot = mdc.load().await.ok().map(Arc::new);
            let _ = tx.send(UiMsg::ModelsReady(
                Box::new(requested_for),
                Box::new(catalog),
                snapshot,
            ));
        });
    }

    /// Launch a run in the background.
    pub fn start_run(&mut self) -> Result<(), String> {
        if let Some(run) = &self.run {
            if run.finished_line.is_none() {
                return Err("a run is already active (press 'c' to cancel)".into());
            }
        }
        let provider = self
            .selected_provider()
            .ok_or_else(|| "select a provider first".to_string())?;
        let model = self
            .selected_model()
            .cloned()
            .ok_or_else(|| "no models loaded — wait for the list or press 'r'".to_string())?;
        if self.task_input.trim().is_empty() {
            return Err("task is empty — type what to build first".into());
        }

        let prompt_path = self.prompts.get(self.prompt_idx).map(|p| p.path.clone());
        let system_prompt = match &prompt_path {
            Some(p) => lmhub_core::load_prompt(p),
            None => lmhub_core::DEFAULT_SYSTEM_PROMPT.to_string(),
        };

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<RunEvent>();
        let app_tx = self.ui_tx.clone();

        let pricing = self.selected_pricing();
        let spec = lmhub_agent::RunSpec {
            provider,
            family_override: model.family.clone(),
            model,
            reasoning: self.selected_reasoning(),
            system_prompt,
            task: self.task_input.trim().to_string(),
            output_base: self.output_base.clone(),
            pricing,
            enable_prompt_cache: true,
            max_turns: self.config.max_turns,
            max_output_tokens: self.config.max_output_tokens,
            deadline: self.config.run_timeout(),
            cancel: CancellationToken::new(),
            sandbox: lmhub_sandbox::SandboxConfig {
                allowed_commands: self.config.allowed_commands.clone(),
                command_timeout: self.config.command_timeout(),
                read_file_max_bytes: self.config.read_file_max_bytes,
                write_file_max_bytes: self.config.write_file_max_bytes,
            },
        };

        let live = ActiveRun {
            provider_id: spec.provider.id().to_string(),
            model_id: spec.model.id.clone(),
            reasoning: spec.reasoning.to_string(),
            started: Instant::now(),
            cancel: spec.cancel.clone(),
            feed_lines: Vec::new(),
            tokens: Usage::default(),
            tool_ok: 0,
            tool_fail: 0,
            errors: 0,
            warnings: 0,
            pricing: spec.pricing.as_ref().map(|c| c.pricing.clone()),
            finished_line: None,
            scroll: 0,
            live_turn: String::new(),
            delta_count: 0,
        };

        let cancel_for_task = spec.cancel.clone();
        tokio::spawn(async move {
            let handle = tokio::spawn(lmhub_agent::execute(spec, Some(event_tx)));
            // Bridge agent events into UI messages.
            while let Some(ev) = event_rx.recv().await {
                let _ = app_tx.send(UiMsg::RunEvent(ev));
            }
            let outcome = handle.await;
            let msg = match outcome {
                Ok(Ok(out)) => UiMsg::RunFinished(Ok(Box::new(out))),
                Ok(Err(e)) => UiMsg::RunFinished(Err(format!("run failed: {e}"))),
                Err(join_err) => UiMsg::RunFinished(Err(format!("task panicked: {join_err}"))),
            };
            let _ = app_tx.send(msg);
            let _ = cancel_for_task; // keep token alive for the span of the task
        });

        self.run = Some(live);
        self.tab = Tab::Run;
        Ok(())
    }

    pub fn handle_ui_msg(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::ModelsReady(requested_for, catalog, snapshot) => {
                self.models_loading = false;
                if self
                    .selected_provider()
                    .map(|p| p.id() != requested_for.as_str())
                    .unwrap_or(true)
                {
                    return; // stale response for a previously selected provider
                }
                self.models = catalog.models.clone();
                self.model_source = catalog.source.map(|s| s.as_str());
                self.model_warnings = catalog.warnings.clone();
                self.snapshot = snapshot;
                self.model_idx = 0;
                self.reasoning_idx = 0;
            }
            UiMsg::RunEvent(ev) => self.apply_run_event(ev),
            UiMsg::RunFinished(result) => {
                if let Some(run) = self.run.as_mut() {
                    run.finished_line = Some(match result {
                        Ok(outcome) => {
                            let s = &outcome.stats;
                            format!(
                                "■ {} — cost {} USD — statistics: {}/statistics.json",
                                s.status.as_str(),
                                s.pricing
                                    .total_usd
                                    .map(|v| format!("{v:.6}"))
                                    .unwrap_or_else(|| "null".into()),
                                outcome.run_dir.display()
                            )
                        }
                        Err(e) => format!("■ runner failure: {e}"),
                    });
                }
            }
            UiMsg::Log(line) => self.push_log(line),
            UiMsg::Notice(line) => self.push_notice(line),
        }
    }

    pub fn push_log(&mut self, line: impl Into<String>) {
        if let Some(run) = self.run.as_mut() {
            run.feed_lines.push(format!("… {}", line.into()));
        }
    }

    fn apply_run_event(&mut self, ev: RunEvent) {
        let Some(run) = self.run.as_mut() else { return };
        match &ev {
            RunEvent::LlmDelta { text, .. } => {
                run.delta_count += 1;
                append_live_tail(&mut run.live_turn, text);
                return; // deltas are UI-only; never rendered as feed lines
            }
            RunEvent::TurnStarted { .. } => {
                run.live_turn.clear();
            }
            RunEvent::ToolCall { status, .. } => {
                // A tool phase starts a new visible segment; count outcome.
                run.live_turn.clear();
                match status.as_str() {
                    "success" => run.tool_ok += 1,
                    _ => run.tool_fail += 1,
                }
            }
            RunEvent::LlmResponse { usage_delta, .. } => {
                run.tokens.add(usage_delta);
                run.live_turn.clear();
            }
            RunEvent::Error { .. } | RunEvent::SandboxViolation { .. } => run.errors += 1,
            RunEvent::Warning { .. } => run.warnings += 1,
            _ => {}
        }
        run.feed_lines.push(ev.to_line());
        if run.feed_lines.len() > 2_000 {
            run.feed_lines.drain(..500);
        }
    }

    pub fn scan_history(&mut self) {
        self.history.clear();
        self.history_detail = None;
        self.history_idx = 0;
        scan_dir_recursive(&self.output_base, 0, &mut self.history);
        self.history.sort_by(|a, b| b.path.cmp(&a.path));
    }
}

fn scan_dir_recursive(dir: &std::path::Path, depth: usize, out: &mut Vec<HistoryRow>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_recursive(&path, depth + 1, out);
        } else if path
            .file_name()
            .map(|n| n == "statistics.json")
            .unwrap_or(false)
        {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                    out.push(HistoryRow {
                        family: v["family"].as_str().unwrap_or("?").into(),
                        model: v["model"].as_str().unwrap_or("?").into(),
                        reasoning: v["reasoning"].as_str().unwrap_or("?").into(),
                        status: v["status"].as_str().unwrap_or("?").into(),
                        duration_ms: v["durationMs"].as_u64(),
                        total_tokens: v["tokens"]["total"].as_u64(),
                        total_usd: v["pricing"]["totalUsd"].as_f64(),
                        path: path.clone(),
                    });
                }
            }
        }
    }
}
