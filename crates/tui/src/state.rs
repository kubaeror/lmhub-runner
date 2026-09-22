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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// Modal overlays on top of a screen.
#[derive(Clone, PartialEq, Eq)]
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

/// Manual `Debug`: a `Modal::EnterKey` holds an API key; it must never be
/// printed by a `{:?}` in a log line.
impl std::fmt::Debug for Modal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EnterKey { provider_id, input } => f
                .debug_struct("EnterKey")
                .field("provider_id", provider_id)
                .field(
                    "input",
                    &format_args!("[redacted; {} chars]", input.as_str().chars().count()),
                )
                .finish(),
            Self::Palette { filter, cursor } => f
                .debug_struct("Palette")
                .field("filter", &filter.as_str())
                .field("cursor", cursor)
                .finish(),
            Self::Help => f.write_str("Help"),
            Self::BulkConfirm => f.write_str("BulkConfirm"),
            Self::HistoryDetail { scroll, .. } => f
                .debug_struct("HistoryDetail")
                .field("scroll", scroll)
                .finish(),
            Self::RunDetail { run_id } => {
                f.debug_struct("RunDetail").field("run_id", run_id).finish()
            }
        }
    }
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
        Self::ORDER[crate::action::rotate(Self::ORDER.len(), idx, forward)]
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
    /// When the run reached a terminal state; elapsed time is frozen here
    /// (instead of counting up forever on the Run screen).
    pub finished_at: Option<Instant>,
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

/// Finished sessions kept in memory for rerun/inspection before pruning.
pub const MAX_FINISHED_SESSIONS: usize = 50;

impl RunRegistry {
    /// Keep the completed backlog bounded. Running/pending sessions are
    /// never dropped; the selection follows its session when possible.
    pub fn prune_finished(&mut self, keep_finished: usize) {
        let finished: Vec<u64> = self
            .runs
            .iter()
            .filter(|r| r.status == RunSessionStatus::Finished)
            .map(|r| r.id)
            .collect();
        if finished.len() <= keep_finished {
            return;
        }
        let drop: std::collections::HashSet<u64> = finished[..finished.len() - keep_finished]
            .iter()
            .copied()
            .collect();
        let selected_id = self.selected_run().map(|r| r.id);
        self.runs.retain(|r| !drop.contains(&r.id));
        self.selected = selected_id
            .and_then(|id| self.runs.iter().position(|r| r.id == id))
            .unwrap_or(0)
            .min(self.runs.len().saturating_sub(1));
    }
}

impl RunSession {
    /// Elapsed wall time, frozen once the run reached a terminal state.
    pub fn elapsed(&self) -> std::time::Duration {
        self.finished_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.started)
    }

    /// Mark a queued session as cancelled (shared by every cancel path).
    pub fn cancel_while_pending(&mut self) {
        self.status = RunSessionStatus::Finished;
        self.finished_at = Some(Instant::now());
        self.finished_line = Some("■ cancelled (was queued)".into());
    }
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
    /// Provider id the current model list belongs to. Ids (not indexes)
    /// survive filtering/reordering: `setup.models` is only ever valid for
    /// this provider.
    pub selected_provider_id: Option<String>,
    /// Model id to re-seat once the next catalog arrives (keeps the user's
    /// place across `r`/F5 reloads and provider round-trips).
    pub restore_model_id: Option<String>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionPrefs {
    pub last_provider: Option<String>,
    pub last_model: Option<String>,
    pub last_reasoning: Option<String>,
    pub last_prompt: Option<String>,
    pub last_task_prompt: Option<String>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub favorites: BTreeSet<String>,
    /// Per-route default reasoning level, keyed `provider_id/model_id`
    /// (older `ui.json` files stored bare model ids; those are migrated on
    /// load).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub model_defaults: std::collections::BTreeMap<String, ReasoningLevel>,
    pub max_concurrent_runs: usize,
}

impl Default for SessionPrefs {
    /// A fresh install allows two concurrent runs (the old `load` special
    /// case); the serde container default uses this too.
    fn default() -> Self {
        Self {
            last_provider: None,
            last_model: None,
            last_reasoning: None,
            last_prompt: None,
            last_task_prompt: None,
            favorites: BTreeSet::new(),
            model_defaults: std::collections::BTreeMap::new(),
            max_concurrent_runs: 2,
        }
    }
}

impl SessionPrefs {
    /// Load prefs, clamping values that cannot work. The optional string is
    /// a user-facing notice when a value had to be corrected.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let mut prefs: SessionPrefs = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        let before = prefs.max_concurrent_runs;
        prefs.sanitize();
        prefs.migrate_legacy_model_defaults();
        let notice = (prefs.max_concurrent_runs != before).then(|| {
            format!(
                "max_concurrent_runs {before} clamped to {}",
                prefs.max_concurrent_runs
            )
        });
        (prefs, notice)
    }

    /// Clamp `max_concurrent_runs` to 1..=64 (also guards hand-edited
    /// ui.json); warns so the correction is visible in the log.
    pub fn sanitize(&mut self) {
        let clamped = self
            .max_concurrent_runs
            .clamp(MIN_CONCURRENT_RUNS, MAX_CONCURRENT_RUNS);
        if clamped != self.max_concurrent_runs {
            tracing::warn!(
                config = "max_concurrent_runs",
                from = self.max_concurrent_runs,
                to = clamped,
                "ui.json value clamped"
            );
            self.max_concurrent_runs = clamped;
        }
    }

    /// Older `ui.json` files keyed `model_defaults` by bare model id. With
    /// per-route keys (`provider/model`) those can only be attributed to the
    /// last selected provider; unattributable entries are dropped rather than
    /// guessed.
    fn migrate_legacy_model_defaults(&mut self) {
        let legacy: Vec<(String, ReasoningLevel)> = self
            .model_defaults
            .iter()
            .filter(|(k, _)| !k.contains('/'))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        if legacy.is_empty() {
            return;
        }
        let provider = self.last_provider.clone();
        for (model, level) in legacy {
            self.model_defaults.remove(&model);
            if let Some(pid) = &provider {
                tracing::debug!(provider = %pid, model = %model, "migrated model default to per-route key");
                self.model_defaults.insert(format!("{pid}/{model}"), level);
            }
        }
    }

    /// Persist atomically (`.tmp` + rename), creating the config dir on a
    /// fresh install. Failures are logged, never fatal.
    pub fn save(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!(
                        path = %parent.display(),
                        error = %e,
                        "could not create config directory for ui.json"
                    );
                    return;
                }
            }
        }
        match serde_json::to_vec_pretty(self) {
            Ok(mut json) => {
                json.push(b'\n');
                if let Err(e) = lmhub_core::util::write_atomic(path, &json) {
                    tracing::warn!(path = %path.display(), error = %e, "could not write ui.json");
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not serialize ui.json"),
        }
    }
}

/// Bounds for `max_concurrent_runs` (also applied to hand-edited ui.json).
pub const MIN_CONCURRENT_RUNS: usize = 1;
pub const MAX_CONCURRENT_RUNS: usize = 64;

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
    /// Cached rendered rows for the current filter + snapshot/catalog state.
    /// Recomputed by [`State::refresh_map_rows`] on every input change so the
    /// view does not rebuild the (potentially large) model list per frame.
    cache: Option<Vec<crate::reasoning_map::MapModel>>,
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
        let (prefs, prefs_notice) = SessionPrefs::load(&prefs_path);

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
        // The setup arrows set reasoning per model; `setup.reasoning` stays the
        // fallback for models without an explicit setting — seed it from the
        // last used level so untouched models get a sensible default.
        if let Some(lvl) = state
            .prefs
            .last_reasoning
            .as_deref()
            .and_then(lmhub_core::ReasoningLevel::parse_effort)
        {
            state.setup.reasoning = lvl;
        }
        if let Some(notice) = prefs_notice {
            state.push_notice(notice);
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
        // Id first: the row index is display/cursor state and may point at a
        // different provider after filtering.
        if let Some(id) = self.setup.selected_provider_id.as_deref() {
            if let Some(p) = self.registry.get(id) {
                return Some(p);
            }
        }
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
    /// level — clamped to what the model actually offers (nearest supported
    /// level, never a hard fallback to off). Mirrors
    /// [`Self::bulk_reasoning_for`] so single and bulk runs agree.
    pub fn selected_reasoning(&self) -> ReasoningLevel {
        let pinned = self
            .selected_provider_id()
            .zip(self.selected_model().map(|m| m.id.clone()))
            .and_then(|(pid, mid)| self.default_reasoning_for(&pid, &mid));
        let level = pinned.unwrap_or(self.setup.reasoning);
        match self.selected_model() {
            Some(m) => level.clamp_to(Some(&levels_for(m))),
            None => level,
        }
    }

    /// The id of the provider the model list currently belongs to.
    pub fn selected_provider_id(&self) -> Option<String> {
        self.setup
            .selected_provider_id
            .clone()
            .or_else(|| self.selected_provider().map(|p| p.id().to_string()))
    }

    /// Re-seat the reasoning display for the current model: a pinned
    /// default seats `reasoning_idx` on fresh visits; otherwise the user's
    /// last chosen level is kept for display. The stored choice
    /// (`setup.reasoning`) is never modified here — it stays the model-
    /// independent fallback for bulk runs — only `reasoning_idx` moves.
    pub fn snap_reasoning_to_default(&mut self) {
        let Some(model) = self.selected_model().map(|m| m.id.clone()) else {
            return;
        };
        let levels = self.visible_reasoning_levels();
        // A freshly selected model with a pinned default seats the display
        // on it (the effective level comes from `selected_reasoning` and
        // `bulk_reasoning_for`, never by clobbering the chosen level).
        let pinned = self
            .selected_provider_id()
            .and_then(|pid| self.default_reasoning_for(&pid, &model));
        if let Some(level) = pinned {
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

    /// Key for a per-route default: `provider/model`.
    pub fn model_default_key(provider_id: &str, model_id: &str) -> String {
        format!("{provider_id}/{model_id}")
    }

    /// The pinned default reasoning for one provider+model route, when set.
    /// Falls back to a legacy bare-model entry from older `ui.json` files.
    pub fn default_reasoning_for(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Option<ReasoningLevel> {
        self.prefs
            .model_defaults
            .get(&Self::model_default_key(provider_id, model_id))
            .copied()
            .or_else(|| self.prefs.model_defaults.get(model_id).copied())
    }

    /// Pin a default reasoning level for one provider+model route.
    pub fn set_model_default_reasoning(
        &mut self,
        provider_id: &str,
        model_id: &str,
        level: ReasoningLevel,
    ) {
        self.prefs.model_defaults.remove(model_id);
        self.prefs
            .model_defaults
            .insert(Self::model_default_key(provider_id, model_id), level);
    }

    /// Reasoning for one bulk spec: the model's pinned default when set,
    /// otherwise the user's last chosen level — always clamped to what the
    /// model supports. Shared by the bulk-confirmation modal and the launch
    /// so the on-screen preview can never disagree with the runs.
    pub fn bulk_reasoning_for(&self, spec: &BulkSpec) -> ReasoningLevel {
        let level = self
            .default_reasoning_for(&spec.provider_id, &spec.model_id)
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

    /// All reasoning-map rows after applying the current filter. Served from
    /// the cache populated by [`Self::refresh_map_rows`] when available.
    pub fn map_rows(&self) -> Vec<crate::reasoning_map::MapModel> {
        if let Some(rows) = &self.map.cache {
            return rows.clone();
        }
        self.compute_map_rows()
    }

    fn compute_map_rows(&self) -> Vec<crate::reasoning_map::MapModel> {
        let Some(snapshot) = &self.snapshot_all else {
            return Vec::new();
        };
        crate::reasoning_map::filtered(
            &crate::reasoning_map::all_models(snapshot, &self.setup.catalog_cache),
            self.map.filter.as_str(),
        )
    }

    /// Recompute the cached map rows. Call after the filter, snapshot or
    /// cached catalogs change.
    pub(crate) fn refresh_map_rows(&mut self) {
        self.map.cache = Some(self.compute_map_rows());
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
                .map(|v| v.len() >= lmhub_core::redact::MIN_SECRET_LEN)
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
        // Keep the user's place across reloads: re-seat the remembered model
        // when it still exists in the new list.
        if let Some(wanted) = self.setup.restore_model_id.clone() {
            if let Some(idx) = self.setup.models.iter().position(|m| m.id == wanted) {
                self.setup.model_idx = idx;
            }
        }
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
        self.setup.selected_provider_id = Some(pid.clone());
        // Switching providers re-seats that provider's last used model.
        self.setup.restore_model_id = self.prefs.last_model.clone();
        let cached = self.setup.catalog_cache.get(&pid).cloned();
        if let Some(cache) = cached {
            self.adopt_catalog(&cache);
            Vec::new()
        } else {
            self.request_models(false)
        }
    }

    /// Re-seat `selected_provider_id` to whatever the (possibly filtered) row
    /// set holds at the cursor and load that provider's models. Keeps
    /// `setup.selected_provider_id` and `setup.models` in lockstep after the
    /// visible rows change.
    pub(crate) fn sync_selected_provider(&mut self) -> Vec<crate::action::Effect> {
        let Some(pid) = self
            .selected_provider_row()
            .and_then(|r| self.registry.all().get(r.registry_idx))
            .map(|p| p.id().to_string())
        else {
            return Vec::new();
        };
        if self.setup.selected_provider_id.as_deref() == Some(pid.as_str()) {
            return Vec::new();
        }
        self.setup.selected_provider_id = Some(pid);
        self.request_models(false)
    }

    /// (Re)load models for the current provider. `force` (F5) bypasses the
    /// Models.dev TTL; plain reload uses the catalog cache when present.
    pub fn request_models(&mut self, force: bool) -> Vec<crate::action::Effect> {
        let Some(provider) = self.selected_provider() else {
            return Vec::new();
        };
        let pid = provider.id().to_string();
        // Remember the current selection so `r`/F5 keeps the user's place.
        self.setup.restore_model_id = self
            .selected_model()
            .map(|m| m.id.clone())
            .or_else(|| self.prefs.last_model.clone());
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
    crate::reasoning_map::offered_levels(
        model.capabilities.reasoning,
        model.capabilities.reasoning_levels.as_deref(),
    )
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

#[cfg(test)]
mod prefs_tests {
    use super::*;

    #[test]
    fn prefs_round_trip_in_a_missing_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config").join("ui.json");
        assert!(
            !path.parent().unwrap().exists(),
            "parent must not exist yet"
        );

        let prefs = SessionPrefs {
            max_concurrent_runs: 3,
            last_provider: Some("openai".into()),
            ..Default::default()
        };
        prefs.save(&path);
        assert!(path.is_file(), "ui.json must be created, parents included");
        assert!(
            !path.with_file_name("ui.json.tmp").exists(),
            "no temp file may be left behind"
        );

        let (loaded, notice) = SessionPrefs::load(&path);
        assert_eq!(loaded.max_concurrent_runs, 3);
        assert_eq!(loaded.last_provider.as_deref(), Some("openai"));
        assert!(notice.is_none());
    }

    #[test]
    fn prefs_clamp_absurd_concurrency_with_a_notice() {
        let mut prefs = SessionPrefs {
            max_concurrent_runs: 0,
            ..Default::default()
        };
        prefs.sanitize();
        assert_eq!(prefs.max_concurrent_runs, MIN_CONCURRENT_RUNS);

        prefs.max_concurrent_runs = 9_999;
        prefs.sanitize();
        assert_eq!(prefs.max_concurrent_runs, MAX_CONCURRENT_RUNS);

        // Through load(): malformed value yields a user-facing notice.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.json");
        std::fs::write(&path, r#"{"max_concurrent_runs": 0}"#).unwrap();
        let (loaded, notice) = SessionPrefs::load(&path);
        assert_eq!(loaded.max_concurrent_runs, MIN_CONCURRENT_RUNS);
        assert!(notice.is_some(), "clamping must be visible to the user");
    }

    #[test]
    fn prefs_default_is_a_sane_two() {
        assert_eq!(SessionPrefs::default().max_concurrent_runs, 2);
    }

    #[test]
    fn setup_levels_match_the_map_rule() {
        use lmhub_core::Capabilities;
        let model = |reasoning: bool, levels: Option<Vec<ReasoningLevel>>| lmhub_core::ModelInfo {
            capabilities: Capabilities {
                reasoning,
                reasoning_budget_min: None,
                reasoning_budget_max: None,
                reasoning_levels: levels,
                ..Default::default()
            },
            ..Default::default()
        };
        for (reasoning, levels) in [
            (false, None),
            (false, Some(vec![])),
            (true, Some(vec![])),
            (true, Some(vec![ReasoningLevel::Off, ReasoningLevel::High])),
            (true, None),
        ] {
            let m = model(reasoning, levels.clone());
            assert_eq!(
                levels_for(&m),
                crate::reasoning_map::offered_levels(reasoning, levels.as_deref()),
                "setup and map must offer the same levels"
            );
        }
    }

    #[test]
    fn modal_debug_redacts_api_keys() {
        let modal = Modal::EnterKey {
            provider_id: "openai".into(),
            input: EditField::from_value("sk-super-secret-value"),
        };
        let rendered = format!("{modal:?}");
        assert!(
            !rendered.contains("sk-super-secret-value"),
            "raw key leaked into Debug: {rendered}"
        );
        assert!(rendered.contains("[redacted"), "{rendered}");
        assert!(rendered.contains("openai"), "{rendered}");
    }
}
