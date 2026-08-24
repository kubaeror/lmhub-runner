//! Application state: everything the TUI knows, plus `reduce` (pure-ish
//! state transitions returning effects) in `reduce.rs`.

use crate::history::HistoryRow;
use crate::input::EditField;
use crate::provider_search::{group_of, Group};
use crate::transcript::Transcript;
use crate::{PromptFile, TuiContext, UiMsg};
use lmhub_core::{
    AppConfig, ModelCatalog, ModelInfo, ModelListSource, ModelPricing, PricingContext, Provider,
    ReasoningLevel, RunEvent, Usage,
};
use lmhub_modelsdev::CatalogSnapshot;
use lmhub_providers::ProviderRegistry;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// Modal overlays on top of a screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// Typing an API key for `provider_id`.
    EnterKey {
        provider_id: String,
        input: EditField,
    },
    /// Command palette with live filter + selection.
    Palette { filter: EditField, cursor: usize },
    /// Keybinding help overlay.
    Help,
    /// Confirmation of a bulk launch (N models across providers).
    BulkConfirm,
    /// Pretty-printed statistics of a previous run (scrollable).
    HistoryDetail { text: String, scroll: usize },
    /// Outcome of a session (final text, paths).
    RunDetail { run_id: u64 },
}

/// Focusable panes on the Setup screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    Providers,
    Models,
    Reasoning,
    Prompts,
    Task,
}

impl Pane {
    pub const ORDER: [Pane; 5] = [
        Pane::Providers,
        Pane::Models,
        Pane::Reasoning,
        Pane::Prompts,
        Pane::Task,
    ];
    pub fn cycle(&self, forward: bool) -> Self {
        let idx = Self::ORDER.iter().position(|p| p == self).unwrap_or(0);
        let next = if forward {
            (idx + 1) % Self::ORDER.len()
        } else {
            (idx + Self::ORDER.len() - 1) % Self::ORDER.len()
        };
        Self::ORDER[next]
    }
}

/// One rendered provider row: either a group header or a provider entry.
#[derive(Debug, Clone, Copy)]
pub struct ProviderRow {
    pub group: Option<Group>,
    pub registry_idx: usize,
}

#[derive(Clone)]
pub struct CachedCatalog {
    pub models: Vec<ModelInfo>,
    pub source: Option<ModelListSource>,
    pub warnings: Vec<String>,
    pub snapshot: Option<Arc<CatalogSnapshot>>,
}

impl CachedCatalog {
    pub(crate) fn from_catalog(
        catalog: &ModelCatalog,
        snapshot: Option<Arc<CatalogSnapshot>>,
    ) -> Self {
        Self {
            models: catalog.models.clone(),
            source: catalog.source,
            warnings: catalog.warnings.clone(),
            snapshot,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunSessionStatus {
    Pending,
    Running,
    Finished,
}

/// One run tracked by the UI (running, queued or done).
pub struct RunSession {
    pub id: u64,
    pub provider_id: String,
    pub model_id: String,
    pub reasoning: String,
    pub task: String,
    pub status: RunSessionStatus,
    pub started: Instant,
    pub cancel: Option<CancellationToken>,
    pub transcript: Transcript,
    pub tokens: Usage,
    pub tool_ok: u64,
    pub tool_fail: u64,
    pub errors: u32,
    pub warnings: u32,
    pub pricing: Option<ModelPricing>,
    pub finished_line: Option<String>,
    pub final_text: Option<String>,
    pub run_dir: Option<PathBuf>,
    pub scroll: usize,
    pub raw_feed: bool,
    pub delta_count: u64,

    // ---- launch payload (kept for pending promotion / rerun) -------------
    pub provider: Arc<dyn Provider>,
    pub model: ModelInfo,
    pub reasoning_level: ReasoningLevel,
    pub system_prompt: String,
    pub pricing_ctx: Option<PricingContext>,
}

pub struct RunRegistry {
    pub runs: Vec<RunSession>,
    pub selected: usize,
    pub next_id: u64,
}

impl RunRegistry {
    pub fn selected_run(&self) -> Option<&RunSession> {
        self.runs.get(self.selected)
    }
    pub fn selected_run_mut(&mut self) -> Option<&mut RunSession> {
        self.runs.get_mut(self.selected)
    }
    pub fn running_count(&self) -> usize {
        self.runs
            .iter()
            .filter(|r| r.status == RunSessionStatus::Running)
            .count()
    }
    pub fn find(&self, id: u64) -> Option<&RunSession> {
        self.runs.iter().find(|r| r.id == id)
    }
    pub fn find_mut(&mut self, id: u64) -> Option<&mut RunSession> {
        self.runs.iter_mut().find(|r| r.id == id)
    }
}

#[derive(Clone, Default)]
pub struct SetupState {
    pub focus: Pane,
    /// Index into the non-header rows of [`State::provider_rows`].
    pub provider_idx: usize,
    /// Live provider search text; empty = browse mode with group headers.
    pub provider_filter: EditField,
    /// Models of the currently selected provider.
    pub models: Vec<ModelInfo>,
    pub models_loading: bool,
    pub model_source: Option<ModelListSource>,
    pub model_warnings: Vec<String>,
    pub model_idx: usize,
    /// Models.dev snapshot for the current provider (pricing lookups).
    pub snapshot: Option<Arc<CatalogSnapshot>>,
    /// Cached catalogs per provider id — enables cross-provider bulk.
    pub catalog_cache: std::collections::BTreeMap<String, CachedCatalog>,
    pub reasoning_idx: usize,
    /// The reasoning level the user last chose, kept **model-independently**
    /// across provider/model switches. This is what single and bulk runs use
    /// (clamped per model); `reasoning_idx` only tracks the display position
    /// for the currently selected model.
    pub reasoning: ReasoningLevel,
    pub prompt_idx: usize,
    /// Index into [`State::task_prompts`] (the Task pane's list).
    pub task_prompt_idx: usize,
    /// Multi-select mode for models (bulk start).
    pub multi_select: bool,
    /// Global (provider_id, model_id) selection — survives provider switches.
    pub bulk: BTreeSet<(String, String)>,
}

/// Preferences persisted to `ui.json` next to the config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionPrefs {
    pub last_provider: Option<String>,
    pub last_model: Option<String>,
    pub last_reasoning: Option<String>,
    pub last_prompt: Option<String>,
    pub last_task_prompt: Option<String>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub favorites: BTreeSet<String>,
    /// Per-model default reasoning level (model id → level).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub model_defaults: std::collections::BTreeMap<String, ReasoningLevel>,
    pub max_concurrent_runs: usize,
}

impl SessionPrefs {
    pub fn load(path: &PathBuf) -> Self {
        let mut prefs: SessionPrefs = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        if prefs.max_concurrent_runs == 0 {
            prefs.max_concurrent_runs = 2;
        }
        prefs
    }
    pub fn save(&self, path: &PathBuf) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

#[derive(Default)]
pub struct HistoryState {
    pub rows: Vec<HistoryRow>,
    pub idx: usize,
}

/// State of the reasoning-map screen (all models across providers).
#[derive(Default)]
pub struct MapState {
    pub filter: EditField,
    pub idx: usize,
}

pub struct State {
    // ---- dependencies (immutable after startup) --------------------------
    pub registry: ProviderRegistry,
    pub modelsdev: Arc<lmhub_modelsdev::ModelsDevClient>,
    pub auth_store: Arc<std::sync::Mutex<lmhub_core::AuthStore>>,
    pub sandbox_runtime: lmhub_sandbox::SandboxRuntime,
    pub config: AppConfig,
    pub config_path: PathBuf,
    pub prefs_path: PathBuf,
    pub prompts: Vec<PromptFile>,
    pub task_prompts: Vec<PromptFile>,
    pub output_base: PathBuf,
    pub ui_tx: UnboundedSender<UiMsg>,

    // ---- global ui state --------------------------------------------------
    pub screen: crate::action::Screen,
    pub modal: Option<Modal>,
    pub notice: Option<(String, Instant)>,
    pub quit: bool,
    pub force_quit: bool,
    /// A cancel request is already in flight (second q force-quits).
    pub cancel_requested: bool,

    pub prefs: SessionPrefs,
    pub setup: SetupState,
    pub runs: RunRegistry,
    pub history: HistoryState,
    pub map: MapState,
    /// Full Models.dev snapshot for the reasoning map (lazy-loaded).
    pub snapshot_all: Option<Arc<CatalogSnapshot>>,
    /// Provider id of the in-flight model fetch (stale-response guard).
    pub requested_models_for: Option<String>,
}

impl State {
    /// Build the app state from the wiring context. The prefs path and the
    /// initial selections are derived from `ctx` + persisted `ui.json`.
    pub fn new(ctx: TuiContext, ui_tx: UnboundedSender<UiMsg>) -> Self {
        let prefs_path = ctx
            .config_path
            .parent()
            .map(|p| p.join("ui.json"))
            .unwrap_or_else(|| ctx.config_path.clone());
        let prefs = SessionPrefs::load(&prefs_path);

        let prompt_idx = ctx
            .config
            .default_prompt
            .as_ref()
            .and_then(|name| ctx.prompts.iter().position(|p| &p.name == name))
            .unwrap_or(0);
        let task_prompt_idx = ctx
            .config
            .default_task_prompt
            .as_ref()
            .and_then(|name| ctx.task_prompts.iter().position(|p| &p.name == name))
            .or_else(|| {
                prefs
                    .last_task_prompt
                    .as_ref()
                    .and_then(|name| ctx.task_prompts.iter().position(|p| &p.name == name))
            })
            .unwrap_or(0);

        let mut state = Self {
            registry: ctx.registry,
            modelsdev: ctx.modelsdev,
            auth_store: ctx.auth_store,
            sandbox_runtime: ctx.sandbox_runtime,
            config: ctx.config,
            config_path: ctx.config_path,
            prefs_path,
            prompts: ctx.prompts,
            task_prompts: ctx.task_prompts,
            output_base: ctx.output_base,
            ui_tx,
            screen: crate::action::Screen::Setup,
            modal: None,
            notice: None,
            quit: false,
            force_quit: false,
            cancel_requested: false,
            prefs,
            setup: SetupState {
                prompt_idx,
                task_prompt_idx,
                ..Default::default()
            },
            runs: RunRegistry {
                runs: Vec::new(),
                selected: 0,
                next_id: 1,
            },
            history: HistoryState::default(),
            map: MapState::default(),
            snapshot_all: None,
            requested_models_for: None,
        };
        // Restore last selections where possible.
        if let Some(pid) = state.prefs.last_provider.clone() {
            if let Some(idx) = state.registry.all().iter().position(|p| p.id() == pid) {
                state.setup.provider_idx = provider_row_index(&state, idx).unwrap_or(0);
            }
        }
        state
    }

    // ---- provider list ---------------------------------------------------

    /// Ordered provider rows: group headers in browse mode, ranked flat list
    /// while searching. `provider_idx` always refers to non-header rows.
    pub fn provider_rows(&self) -> Vec<ProviderRow> {
        let all = self.registry.all();
        let filter = self.setup.provider_filter.as_str().trim();
        if filter.is_empty() {
            let mut rows: Vec<ProviderRow> = Vec::new();
            for group in [Group::Native, Group::Routed, Group::Local] {
                let mut members: Vec<usize> = all
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| group_of(p.as_ref()) == group)
                    .map(|(i, _)| i)
                    .collect();
                if members.is_empty() {
                    continue;
                }
                members.sort_by_key(|i| {
                    (
                        !self.prefs.favorites.contains(all[*i].id()),
                        all[*i].display_name().to_ascii_lowercase(),
                    )
                });
                rows.push(ProviderRow {
                    group: Some(group),
                    registry_idx: 0,
                });
                rows.extend(members.into_iter().map(|i| ProviderRow {
                    group: None,
                    registry_idx: i,
                }));
            }
            rows
        } else {
            let mut scored: Vec<(u8, usize, usize)> = all
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    crate::provider_search::rank_provider(filter, p.as_ref())
                        .map(|s| (s.kind, s.start, i))
                })
                .collect();
            scored.sort_by_key(|(kind, start, i)| {
                (
                    *kind,
                    *start,
                    !self.prefs.favorites.contains(all[*i].id()),
                    all[*i].display_name().to_ascii_lowercase(),
                )
            });
            scored
                .into_iter()
                .map(|(_, _, i)| ProviderRow {
                    group: None,
                    registry_idx: i,
                })
                .collect()
        }
    }

    pub fn selected_provider_row(&self) -> Option<ProviderRow> {
        self.provider_rows()
            .into_iter()
            .filter(|r| r.group.is_none())
            .nth(self.setup.provider_idx)
    }

    pub fn selected_provider(&self) -> Option<Arc<dyn Provider>> {
        let row = self.selected_provider_row()?;
        self.registry.all().get(row.registry_idx).cloned()
    }

    pub fn selected_model(&self) -> Option<&ModelInfo> {
        self.setup.models.get(self.setup.model_idx)
    }

    pub fn visible_reasoning_levels(&self) -> Vec<ReasoningLevel> {
        match self.selected_model() {
            Some(m) => levels_for(m),
            None => vec![ReasoningLevel::Off],
        }
    }

    /// The effective reasoning level for the selected model: its pinned
    /// per-model default when one is set, otherwise the user's last chosen
    /// level — clamped to what the model actually offers (its first level
    /// when the choice is unsupported — never a hard fallback to off).
    /// Mirrors [`Self::bulk_reasoning_for`] so single and bulk runs agree.
    pub fn selected_reasoning(&self) -> ReasoningLevel {
        let level = self
            .selected_model()
            .and_then(|m| self.default_reasoning_for(&m.id))
            .unwrap_or(self.setup.reasoning);
        let levels = self.visible_reasoning_levels();
        if levels.contains(&level) {
            level
        } else {
            levels.first().copied().unwrap_or(ReasoningLevel::Off)
        }
    }

    /// Re-seat the reasoning display for the current model: a pinned
    /// default seats `reasoning_idx` on fresh visits; otherwise the user's
    /// last chosen level is kept for display. The stored choice
    /// (`setup.reasoning`) is never modified here — it stays the model-
    /// independent fallback for bulk runs — only `reasoning_idx` moves.
    pub fn snap_reasoning_to_default(&mut self) {
        let Some(model) = self.selected_model() else {
            return;
        };
        let levels = self.visible_reasoning_levels();
        // A freshly selected model with a pinned default seats the display
        // on it (the effective level comes from `selected_reasoning` and
        // `bulk_reasoning_for`, never by clobbering the chosen level).
        if let Some(level) = self.prefs.model_defaults.get(&model.id).copied() {
            if let Some(idx) = levels.iter().position(|l| *l == level) {
                self.setup.reasoning_idx = idx;
                return;
            }
        }
        // Otherwise keep the user's choice when the model supports it.
        if let Some(idx) = levels.iter().position(|l| *l == self.setup.reasoning) {
            self.setup.reasoning_idx = idx;
        } else {
            self.setup.reasoning_idx = 0;
        }
    }

    /// The default reasoning for a model, when set. `None` = model default
    /// or "whatever the current selection says".
    pub fn default_reasoning_for(&self, model_id: &str) -> Option<ReasoningLevel> {
        self.prefs.model_defaults.get(model_id).copied()
    }

    /// Reasoning for one bulk spec: the model's pinned default when set,
    /// otherwise the user's last chosen level — always clamped to what the
    /// model supports. Shared by the bulk-confirmation modal and the launch
    /// so the on-screen preview can never disagree with the runs.
    pub fn bulk_reasoning_for(&self, spec: &BulkSpec) -> ReasoningLevel {
        let level = self
            .default_reasoning_for(&spec.model_id)
            .unwrap_or(self.setup.reasoning);
        level.clamp_to(Some(&levels_for(&spec.model)))
    }

    /// Selected model row in the reasoning map (filter-aware).
    pub fn selected_map_model(&self) -> Option<crate::reasoning_map::MapModel> {
        let snapshot = self.snapshot_all.as_ref()?;
        let rows = crate::reasoning_map::filtered(
            &crate::reasoning_map::all_models(snapshot, &self.setup.catalog_cache),
            self.map.filter.as_str(),
        );
        rows.get(self.map.idx).cloned()
    }

    /// All reasoning-map rows after applying the current filter.
    pub fn map_rows(&self) -> Vec<crate::reasoning_map::MapModel> {
        let Some(snapshot) = &self.snapshot_all else {
            return Vec::new();
        };
        crate::reasoning_map::filtered(
            &crate::reasoning_map::all_models(snapshot, &self.setup.catalog_cache),
            self.map.filter.as_str(),
        )
    }

    pub fn selected_pricing(&self) -> Option<PricingContext> {
        let snapshot = self.setup.snapshot.as_ref()?;
        let provider = self.selected_provider()?;
        let hint = provider.models_dev_hint();
        let model = self.selected_model()?;
        lmhub_providers::pricing_context_in_snapshot(snapshot, hint, &model.id)
    }

    // ---- helpers ---------------------------------------------------------

    pub fn push_notice(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), Instant::now()));
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

    /// Adopt a cached catalog as the current provider's model list.
    pub fn adopt_catalog(&mut self, cache: &CachedCatalog) {
        self.setup.models = cache.models.clone();
        self.setup.model_source = cache.source;
        self.setup.model_warnings = cache.warnings.clone();
        self.setup.snapshot = cache.snapshot.clone();
        self.setup.models_loading = false;
        self.setup.model_idx = 0;
        self.setup.reasoning_idx = 0;
        self.snap_reasoning_to_default();
    }

    /// Select the provider at a non-header row index; loads models unless
    /// the catalog is already cached. Returns effects to run.
    pub(crate) fn select_provider(&mut self, row_idx: usize) -> Vec<crate::action::Effect> {
        let Some(pid) = self
            .provider_rows()
            .into_iter()
            .filter(|r| r.group.is_none())
            .nth(row_idx)
            .and_then(|r| {
                self.registry
                    .all()
                    .get(r.registry_idx)
                    .map(|p| p.id().to_string())
            })
        else {
            return Vec::new();
        };
        self.setup.provider_idx = row_idx;
        let cached = self.setup.catalog_cache.get(&pid).cloned();
        if let Some(cache) = cached {
            self.adopt_catalog(&cache);
            Vec::new()
        } else {
            let provider = self.selected_provider().expect("row just resolved");
            self.setup.models.clear();
            self.setup.model_idx = 0;
            self.setup.models_loading = true;
            self.requested_models_for = Some(pid);
            vec![crate::action::Effect::FetchModels {
                provider,
                force: false,
            }]
        }
    }

    /// (Re)load models for the current provider. `force` (F5) bypasses the
    /// Models.dev TTL; plain reload uses the catalog cache when present.
    pub fn request_models(&mut self, force: bool) -> Vec<crate::action::Effect> {
        let Some(provider) = self.selected_provider() else {
            return Vec::new();
        };
        let pid = provider.id().to_string();
        if !force {
            let cached = self.setup.catalog_cache.get(&pid).cloned();
            if let Some(cache) = cached {
                self.adopt_catalog(&cache);
                return Vec::new();
            }
        }
        self.setup.models.clear();
        self.setup.model_idx = 0;
        self.setup.models_loading = true;
        self.requested_models_for = Some(pid);
        vec![crate::action::Effect::FetchModels { provider, force }]
    }

    /// How many models are checked for a provider (☑ badge in the list).
    pub fn bulk_count_for(&self, provider_id: &str) -> usize {
        self.setup
            .bulk
            .iter()
            .filter(|(pid, _)| pid == provider_id)
            .count()
    }

    /// Models currently checked for the *selected* provider.
    pub fn bulk_checked_indices(&self) -> Vec<usize> {
        let Some(pid) = self.selected_provider().map(|p| p.id().to_string()) else {
            return Vec::new();
        };
        self.setup
            .models
            .iter()
            .enumerate()
            .filter(|(_, m)| self.setup.bulk.contains(&(pid.clone(), m.id.clone())))
            .map(|(i, _)| i)
            .collect()
    }

    /// Resolve the bulk selection into launchable specs. Providers whose
    /// catalog was never loaded (selection made, provider never visited with
    /// models cached) are skipped.
    pub fn bulk_specs(&self) -> Vec<BulkSpec> {
        let mut specs: Vec<BulkSpec> = Vec::new();
        for (pid, mid) in &self.setup.bulk {
            let Some(provider) = self.registry.get(pid) else {
                continue;
            };
            let Some(cache) = self.setup.catalog_cache.get(pid) else {
                continue;
            };
            let Some(model) = cache.models.iter().find(|m| &m.id == mid).cloned() else {
                continue;
            };
            let pricing = cache.snapshot.as_ref().and_then(|snap| {
                lmhub_providers::pricing_context_in_snapshot(
                    snap,
                    provider.models_dev_hint(),
                    &model.id,
                )
            });
            specs.push(BulkSpec {
                provider_id: pid.clone(),
                model_id: mid.clone(),
                model,
                pricing,
            });
        }
        specs.sort_by(|a, b| {
            a.provider_id
                .cmp(&b.provider_id)
                .then_with(|| a.model_id.cmp(&b.model_id))
        });
        specs
    }
}

/// Non-header row index of `registry_idx` in the current browse ordering.
fn provider_row_index(state: &State, registry_idx: usize) -> Option<usize> {
    state
        .provider_rows()
        .into_iter()
        .filter(|r| r.group.is_none())
        .position(|r| r.registry_idx == registry_idx)
}

/// Levels a model actually offers, per its capabilities declaration — the
/// same semantics as the setup pane: no reasoning → off only; empty
/// declaration → off only; no declaration → all levels.
fn levels_for(model: &ModelInfo) -> Vec<ReasoningLevel> {
    if !model.capabilities.reasoning {
        return vec![ReasoningLevel::Off];
    }
    match &model.capabilities.reasoning_levels {
        Some(levels) if levels.is_empty() => vec![ReasoningLevel::Off],
        Some(levels) => levels.clone(),
        None => ReasoningLevel::ALL.to_vec(),
    }
}

/// One (provider, model) pair from the bulk selection, resolved to a model
/// + pricing so the confirmation modal and the launch can use it.
#[derive(Debug, Clone)]
pub struct BulkSpec {
    pub provider_id: String,
    pub model_id: String,
    pub model: ModelInfo,
    pub pricing: Option<PricingContext>,
}

/// Feed a raw run event into a session; used by `reduce`.
pub fn fold_run_event(state: &mut State, run_id: u64, ev: RunEvent) {
    let Some(run) = state.runs.find_mut(run_id) else {
        return;
    };
    run.transcript.fold(&ev);
    match &ev {
        RunEvent::LlmDelta { .. } => run.delta_count += 1,
        RunEvent::ToolCall { status, .. } => {
            if status.as_str() == "success" {
                run.tool_ok += 1;
            } else {
                run.tool_fail += 1;
            }
        }
        RunEvent::LlmResponse { usage_delta, .. } => run.tokens.add(usage_delta),
        RunEvent::Error { .. } | RunEvent::SandboxViolation { .. } => run.errors += 1,
        RunEvent::Warning { .. } => run.warnings += 1,
        _ => {}
    }
}
