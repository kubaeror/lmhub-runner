//! `State::reduce` — the single place where actions mutate state. Returns
//! effects for the event loop (async fetches/launches, persistence).
//!
//! Deliberately "pure-ish": small synchronous IO (config/ui.json saves,
//! history reads) happens inline; anything async or long-running becomes an
//! [`Effect`] whose results come back as `Action::UiMsg`.

use crate::action::{Action, Effect, Screen};
use crate::history;
use crate::state::{fold_run_event, Modal, Pane, RunSession, RunSessionStatus, State};
use crate::transcript::Transcript;
use lmhub_core::StoredCredential;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Command palette entries: what the `:` menu can do right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCmd {
    RunSingle,
    BulkRun,
    CancelAllRuns,
    RescanHistory,
    OpenOutputDir,
    Quit,
}

impl PaletteCmd {
    pub fn label(&self) -> &'static str {
        match self {
            Self::RunSingle => "Run current selection",
            Self::BulkRun => "Bulk run selected models",
            Self::CancelAllRuns => "Cancel all runs",
            Self::RescanHistory => "Rescan history",
            Self::OpenOutputDir => "Open output directory",
            Self::Quit => "Quit",
        }
    }
}

impl State {
    pub fn reduce(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::Quit => self.action_quit(),
            Action::ForceQuit => {
                self.quit = true;
                self.force_quit = true;
                self.save_prefs();
                vec![Effect::SavePrefs]
            }
            Action::SwitchScreen(screen) => {
                self.screen = screen;
                self.modal = None;
                let mut effects = Vec::new();
                if screen == Screen::History && self.history.rows.is_empty() {
                    effects.push(Effect::ScanHistory);
                }
                if screen == Screen::Reasoning && self.snapshot_all.is_none() {
                    effects.push(Effect::LoadSnapshot);
                }
                if screen == Screen::Run {
                    self.runs.selected = self
                        .runs
                        .selected
                        .min(self.runs.runs.len().saturating_sub(1));
                }
                effects
            }
            Action::OpenPalette => {
                self.modal = Some(Modal::Palette {
                    filter: String::new(),
                    cursor: 0,
                });
                Vec::new()
            }
            Action::CloseModal => {
                self.modal = None;
                Vec::new()
            }
            Action::Notice(msg) => {
                self.push_notice(msg);
                Vec::new()
            }
            Action::UiMsg(msg) => self.reduce_ui_msg(msg),

            // ---- setup -----------------------------------------------------
            Action::CycleFocus(forward) => {
                self.setup.focus = self.setup.focus.cycle(forward);
                Vec::new()
            }
            Action::FocusPane(pane) => {
                self.setup.focus = pane;
                Vec::new()
            }
            Action::EnterKeyChar(c) => {
                if !c.is_control() {
                    if let Some(Modal::EnterKey { input, .. }) = &mut self.modal {
                        input.push(c);
                    }
                }
                Vec::new()
            }
            Action::EnterKeyBackspace => {
                if let Some(Modal::EnterKey { input, .. }) = &mut self.modal {
                    input.pop();
                }
                Vec::new()
            }
            Action::SaveKey => {
                self.save_entered_key();
                Vec::new()
            }
            Action::MoveSelection(delta) => match self.setup.focus {
                Pane::Providers => self.move_provider(delta),
                Pane::Models => self.move_model(delta),
                Pane::Prompts => self.move_prompt(delta),
                _ => Vec::new(),
            },
            Action::SearchProviders(text) => {
                self.setup.provider_filter = text;
                self.clamp_provider_idx();
                Vec::new()
            }
            Action::ClearSearch => {
                self.setup.provider_filter.clear();
                self.clamp_provider_idx();
                Vec::new()
            }
            Action::ToggleFavorite => self.toggle_favorite(),
            Action::ConnectProvider => self.connect_provider(),
            Action::SelectModel => {
                if let Some(m) = self.selected_model() {
                    self.prefs.last_model = Some(m.id.clone());
                    vec![Effect::SavePrefs]
                } else {
                    Vec::new()
                }
            }
            Action::ToggleMultiSelect => {
                self.setup.multi_select = !self.setup.multi_select;
                Vec::new()
            }
            Action::ToggleBulk => self.toggle_bulk(),
            Action::ClearBulk => {
                self.setup.bulk.clear();
                Vec::new()
            }
            Action::CycleReasoning(delta) => {
                let levels = self.visible_reasoning_levels();
                let len = levels.len().max(1);
                self.setup.reasoning_idx =
                    (self.setup.reasoning_idx as i32 + delta).rem_euclid(len as i32) as usize;
                Vec::new()
            }
            Action::CyclePrompt(delta) => {
                let len = self.prompts.len().max(1);
                self.setup.prompt_idx =
                    (self.setup.prompt_idx as i32 + delta).rem_euclid(len as i32) as usize;
                Vec::new()
            }
            Action::SetDefaultPrompt => self.set_default_prompt(),
            Action::TaskChar(c) => {
                if c.is_control() {
                    return Vec::new();
                }
                self.setup.task_input.push(c);
                self.setup.task_recall_idx = None;
                Vec::new()
            }
            Action::TaskBackspace => {
                self.setup.task_input.pop();
                self.setup.task_recall_idx = None;
                Vec::new()
            }
            Action::TaskRecall(delta) => self.task_recall(delta),
            Action::StartRun => self.start_run(),
            Action::BulkStart => self.bulk_start(),
            Action::ConfirmBulkStart => self.confirm_bulk_start(),
            Action::RefreshModels(force) => self.request_models(force),

            // ---- run -------------------------------------------------------
            Action::NextSession => {
                if self.runs.selected + 1 < self.runs.runs.len() {
                    self.runs.selected += 1;
                }
                Vec::new()
            }
            Action::PrevSession => {
                self.runs.selected = self.runs.selected.saturating_sub(1);
                Vec::new()
            }
            Action::ScrollTranscript(delta) => {
                if let Some(run) = self.runs.selected_run_mut() {
                    run.scroll = run.scroll.saturating_add(delta.max(0) as usize);
                    if delta < 0 {
                        run.scroll = run.scroll.saturating_sub((-delta) as usize);
                    }
                }
                Vec::new()
            }
            Action::CancelSession => {
                let mut notice = None;
                if let Some(run) = self.runs.selected_run_mut() {
                    match run.status {
                        RunSessionStatus::Pending => {
                            run.status = RunSessionStatus::Finished;
                            run.finished_line = Some("■ cancelled (was queued)".into());
                        }
                        RunSessionStatus::Running => {
                            if let Some(c) = &run.cancel {
                                c.cancel();
                            }
                            notice =
                                Some(format!("cancelling {} — {}", run.provider_id, run.model_id));
                        }
                        RunSessionStatus::Finished => {}
                    }
                }
                if let Some(n) = notice {
                    self.push_notice(n);
                }
                Vec::new()
            }
            Action::CancelAllRuns => {
                let mut cancelled = 0;
                for run in &mut self.runs.runs {
                    match run.status {
                        RunSessionStatus::Running => {
                            if let Some(c) = &run.cancel {
                                c.cancel();
                            }
                            cancelled += 1;
                        }
                        RunSessionStatus::Pending => {
                            run.status = RunSessionStatus::Finished;
                            run.finished_line = Some("■ cancelled (was queued)".into());
                        }
                        RunSessionStatus::Finished => {}
                    }
                }
                if cancelled > 0 {
                    self.push_notice(format!("cancelling {cancelled} running run(s)"));
                }
                Vec::new()
            }
            Action::RerunSession => {
                let mut effects = Vec::new();
                if let Some(run) = self.runs.selected_run_mut() {
                    if run.status == RunSessionStatus::Finished {
                        let cancel = CancellationToken::new();
                        run.status = RunSessionStatus::Running;
                        run.cancel = Some(cancel.clone());
                        run.started = Instant::now();
                        run.transcript = Transcript::default();
                        run.tokens = Default::default();
                        run.tool_ok = 0;
                        run.tool_fail = 0;
                        run.errors = 0;
                        run.warnings = 0;
                        run.finished_line = None;
                        run.final_text = None;
                        run.run_dir = None;
                        run.scroll = 0;
                        run.delta_count = 0;
                        effects.push(Effect::LaunchRun { run_id: run.id });
                    }
                }
                effects
            }
            Action::ToggleRawFeed => {
                if let Some(run) = self.runs.selected_run_mut() {
                    run.raw_feed = !run.raw_feed;
                }
                Vec::new()
            }
            Action::OpenRunDetail => {
                if let Some(run) = self.runs.selected_run() {
                    self.modal = Some(Modal::RunDetail { run_id: run.id });
                }
                Vec::new()
            }

            // ---- history ---------------------------------------------------
            Action::MoveHistory(delta) => {
                let len = self.history.rows.len();
                if len > 0 {
                    self.history.idx =
                        ((self.history.idx as i32 + delta).max(0) as usize).min(len - 1);
                }
                Vec::new()
            }
            Action::RescanHistory => vec![Effect::ScanHistory],
            Action::OpenHistoryDetail => {
                if let Some(row) = self.history.rows.get(self.history.idx) {
                    match history::read_detail(&row.path) {
                        Ok(text) => self.modal = Some(Modal::HistoryDetail(text)),
                        Err(e) => self.push_notice(e),
                    }
                }
                Vec::new()
            }

            // ---- reasoning map -------------------------------------
            Action::MapFilter(text) => {
                self.map.filter = text;
                self.map.idx = self.map.idx.min(self.map_rows().len().saturating_sub(1));
                Vec::new()
            }
            Action::MapClear => {
                self.map.filter.clear();
                self.map.idx = 0;
                Vec::new()
            }
            Action::MapMove(delta) => {
                let len = self.map_rows().len();
                if len > 0 {
                    self.map.idx = ((self.map.idx as i32 + delta).max(0) as usize).min(len - 1);
                }
                Vec::new()
            }
            Action::CycleModelDefault => self.cycle_model_default(),
            Action::SetModelDefault => self.set_model_default(),
            Action::ReloadSnapshot => {
                self.snapshot_all = None;
                vec![Effect::LoadSnapshot]
            }

            // ---- palette ---------------------------------------------------
            Action::PaletteChar(c) => {
                if c.is_control() {
                    return Vec::new();
                }
                if let Some(Modal::Palette { filter, .. }) = &mut self.modal {
                    filter.push(c);
                }
                Vec::new()
            }
            Action::PaletteBackspace => {
                if let Some(Modal::Palette { filter, .. }) = &mut self.modal {
                    filter.pop();
                }
                Vec::new()
            }
            Action::PaletteMove(delta) => {
                let len = self.palette_commands().len().max(1);
                if let Some(Modal::Palette { cursor, .. }) = &mut self.modal {
                    *cursor = (*cursor as i32 + delta).rem_euclid(len as i32) as usize;
                }
                Vec::new()
            }
            Action::PaletteEnter => self.palette_enter(),
            Action::PaletteRunAction(idx) => self.palette_run(idx),
        }
    }

    // ---- global -----------------------------------------------------------

    fn action_quit(&mut self) -> Vec<Effect> {
        let active = self
            .runs
            .runs
            .iter()
            .any(|r| r.status != RunSessionStatus::Finished);
        if active {
            for run in &mut self.runs.runs {
                if run.status == RunSessionStatus::Running {
                    if let Some(c) = &run.cancel {
                        c.cancel();
                    }
                } else if run.status == RunSessionStatus::Pending {
                    run.status = RunSessionStatus::Finished;
                    run.finished_line = Some("■ cancelled (was queued)".into());
                }
            }
            self.cancel_requested = true;
            self.push_notice("cancel requested — press q again to force quit");
        } else {
            self.quit = true;
        }
        self.save_prefs();
        vec![Effect::SavePrefs]
    }

    /// Snapshot current selections into prefs (persisted via Effect).
    pub fn save_prefs(&mut self) {
        if let Some(p) = self.selected_provider() {
            self.prefs.last_provider = Some(p.id().to_string());
        }
        if let Some(m) = self.selected_model() {
            self.prefs.last_model = Some(m.id.clone());
        }
        self.prefs.last_reasoning = Some(self.selected_reasoning().as_str().to_string());
        if let Some(p) = self.prompts.get(self.setup.prompt_idx) {
            self.prefs.last_prompt = Some(p.name.clone());
        }
        if !self.setup.task_input.trim().is_empty() {
            self.prefs.last_task = Some(self.setup.task_input.clone());
        }
    }

    // ---- ui messages (effect results) -------------------------------------

    fn reduce_ui_msg(&mut self, msg: crate::UiMsg) -> Vec<Effect> {
        match msg {
            crate::UiMsg::ModelsReady {
                requested_for,
                catalog,
                snapshot,
            } => {
                // Cache under the provider it was fetched for — enables
                // cross-provider bulk without refetching.
                let cache = crate::state::CachedCatalog::from_catalog(&catalog, snapshot.clone());
                let is_current = self
                    .requested_models_for
                    .as_deref()
                    .map(|r| r == requested_for.as_str())
                    .unwrap_or(false);
                self.setup
                    .catalog_cache
                    .insert(requested_for.clone(), cache);
                if is_current {
                    self.requested_models_for = None;
                    self.setup.models_loading = false;
                    if self
                        .selected_provider()
                        .map(|p| p.id() == requested_for.as_str())
                        .unwrap_or(false)
                    {
                        let cached = self.setup.catalog_cache.get(&requested_for).cloned();
                        if let Some(cache) = cached {
                            self.adopt_catalog(&cache);
                        }
                        self.restore_last_model();
                        self.snap_reasoning_to_default();
                    }
                }
                Vec::new()
            }
            crate::UiMsg::RunEvent { run_id, event } => {
                fold_run_event(self, run_id, event);
                Vec::new()
            }
            crate::UiMsg::RunFinished { run_id, result } => {
                let mut failure_notice = None;
                if let Some(run) = self.runs.find_mut(run_id) {
                    run.status = RunSessionStatus::Finished;
                    run.finished_line = Some(match &result {
                        Ok(outcome) => {
                            let s = &outcome.stats;
                            run.final_text = outcome.final_text.clone();
                            run.run_dir = Some(outcome.run_dir.clone());
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
                        Err(e) => {
                            // Surface the failure immediately, even when the
                            // user is on another screen.
                            failure_notice = Some(format!("✖ run failed: {e}"));
                            format!("■ runner failure: {e}")
                        }
                    });
                }
                if let Some(notice) = failure_notice {
                    self.push_notice(notice);
                }
                // A slot freed up: promote queued runs.
                self.promote_pending()
            }
            crate::UiMsg::SnapshotLoaded(snapshot) => {
                self.snapshot_all = Some(snapshot);
                self.map.idx = self.map.idx.min(self.map_rows().len().saturating_sub(1));
                Vec::new()
            }
            crate::UiMsg::Notice(msg) => {
                self.push_notice(msg);
                Vec::new()
            }
        }
    }

    /// Promote queued runs up to the concurrency cap. Returns launch effects.
    fn promote_pending(&mut self) -> Vec<Effect> {
        let cap = self.prefs.max_concurrent_runs.max(1);
        let mut effects = Vec::new();
        let mut promoted: Vec<String> = Vec::new();
        loop {
            if self.runs.running_count() >= cap {
                break;
            }
            let Some(pending) = self
                .runs
                .runs
                .iter_mut()
                .find(|r| r.status == RunSessionStatus::Pending)
            else {
                break;
            };
            pending.status = RunSessionStatus::Running;
            pending.started = Instant::now();
            pending.cancel = Some(CancellationToken::new());
            let id = pending.id;
            let model = pending.model_id.clone();
            effects.push(Effect::LaunchRun { run_id: id });
            promoted.push(model);
        }
        for model in promoted {
            self.push_notice(format!("queued run {model} started"));
        }
        effects
    }

    fn restore_last_model(&mut self) {
        if let Some(mid) = &self.prefs.last_model {
            if let Some(idx) = self.setup.models.iter().position(|m| &m.id == mid) {
                self.setup.model_idx = idx;
            }
        }
    }

    // ---- setup ------------------------------------------------------------

    fn clamp_provider_idx(&mut self) {
        let count = self
            .provider_rows()
            .into_iter()
            .filter(|r| r.group.is_none())
            .count();
        self.setup.provider_idx = self.setup.provider_idx.min(count.saturating_sub(1));
    }

    fn move_provider(&mut self, delta: i32) -> Vec<Effect> {
        let count = self
            .provider_rows()
            .into_iter()
            .filter(|r| r.group.is_none())
            .count();
        if count == 0 {
            return Vec::new();
        }
        let prev = self.setup.provider_idx.min(count - 1);
        let next = (prev as i32 + delta).clamp(0, count as i32 - 1) as usize;
        if next == prev {
            return Vec::new();
        }
        self.select_provider(next)
    }
    fn move_model(&mut self, delta: i32) -> Vec<Effect> {
        let len = self.setup.models.len();
        if len == 0 {
            return Vec::new();
        }
        self.setup.model_idx = ((self.setup.model_idx as i32 + delta).max(0) as usize).min(len - 1);
        self.setup.reasoning_idx = 0;
        self.snap_reasoning_to_default();
        Vec::new()
    }

    fn move_prompt(&mut self, delta: i32) -> Vec<Effect> {
        let len = self.prompts.len();
        if len == 0 {
            return Vec::new();
        }
        self.setup.prompt_idx =
            ((self.setup.prompt_idx as i32 + delta).max(0) as usize).min(len - 1);
        Vec::new()
    }

    fn toggle_favorite(&mut self) -> Vec<Effect> {
        if let Some(p) = self.selected_provider() {
            if !self.prefs.favorites.remove(p.id()) {
                self.prefs.favorites.insert(p.id().to_string());
            }
            self.push_notice(format!(
                "★ {}",
                if self.prefs.favorites.contains(p.id()) {
                    format!("{} is now a favorite", p.display_name())
                } else {
                    format!("{} is no longer a favorite", p.display_name())
                }
            ));
            // Re-clamp: ordering changed (favorites sort first).
            self.clamp_provider_idx();
            vec![Effect::SavePrefs]
        } else {
            Vec::new()
        }
    }

    fn connect_provider(&mut self) -> Vec<Effect> {
        let Some(provider) = self.selected_provider() else {
            return Vec::new();
        };
        let id = provider.id().to_string();
        if id == "github-copilot" {
            self.begin_copilot_flow();
            Vec::new()
        } else {
            self.modal = Some(Modal::EnterKey {
                provider_id: id,
                input: String::new(),
            });
            Vec::new()
        }
    }

    fn begin_copilot_flow(&mut self) {
        let tx = self.ui_tx.clone();
        let auth_store = std::sync::Arc::clone(&self.auth_store);
        self.push_notice("copilot: starting device flow…");
        tokio::spawn(async move {
            let result = lmhub_providers::copilot::run_full_flow(&auth_store, |line| {
                let _ = tx.send(crate::UiMsg::Notice(line));
            })
            .await;
            if let Err(e) = result {
                let _ = tx.send(crate::UiMsg::Notice(format!("✖ copilot: {e}")));
            }
        });
    }

    /// `Enter` inside the key-entry modal: validate + persist the key.
    pub fn save_entered_key(&mut self) -> bool {
        let Some(Modal::EnterKey { provider_id, input }) = &self.modal else {
            return false;
        };
        let id = provider_id.clone();
        let key = input.trim().to_string();
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
                self.modal = None;
                self.push_notice(format!("saved key for {id}"));
                true
            }
            Err(e) => {
                self.push_notice(format!("✖ saving auth.json failed: {e}"));
                false
            }
        }
    }

    fn toggle_bulk(&mut self) -> Vec<Effect> {
        let Some(pid) = self.selected_provider().map(|p| p.id().to_string()) else {
            return Vec::new();
        };
        let Some(model) = self.selected_model().cloned() else {
            return Vec::new();
        };
        if !self.setup.multi_select {
            self.setup.multi_select = true;
        }
        let key = (pid, model.id);
        if !self.setup.bulk.remove(&key) {
            self.setup.bulk.insert(key);
        }
        Vec::new()
    }

    fn task_recall(&mut self, delta: i32) -> Vec<Effect> {
        let hist = &self.prefs.task_history;
        if hist.is_empty() {
            return Vec::new();
        }
        let len = hist.len();
        // History is ordered oldest → newest; Up (delta > 0) walks older.
        let pos = match self.setup.task_recall_idx {
            Some(p) if delta > 0 => p.saturating_sub(1),
            Some(p) => (p + 1).min(len - 1),
            None if delta > 0 => len - 1,
            None => 0,
        };
        self.setup.task_recall_idx = Some(pos);
        self.setup.task_input = hist[pos].clone();
        Vec::new()
    }

    fn set_default_prompt(&mut self) -> Vec<Effect> {
        let Some(p) = self.prompts.get(self.setup.prompt_idx) else {
            return Vec::new();
        };
        self.config.default_prompt = Some(p.name.clone());
        match self.config.save(&self.config_path) {
            Ok(()) => self.push_notice(format!("default prompt → {}", p.name)),
            Err(e) => self.push_notice(format!("✖ could not save config: {e}")),
        }
        self.save_prefs();
        vec![Effect::SavePrefs]
    }

    /// `d` in the Setup Reasoning pane: pin the current level as the
    /// current model's default (persisted, used on future selections).
    fn set_model_default(&mut self) -> Vec<Effect> {
        let Some(model_id) = self.selected_model().map(|m| m.id.clone()) else {
            self.push_notice("select a model first");
            return Vec::new();
        };
        let level = self.selected_reasoning();
        self.prefs.model_defaults.insert(model_id.clone(), level);
        self.push_notice(format!(
            "default reasoning for {} → {}",
            model_id,
            level.as_str()
        ));
        vec![Effect::SavePrefs]
    }

    /// `d` in the Reasoning map: cycle the selected model's default
    /// through its supported levels (wrapping).
    fn cycle_model_default(&mut self) -> Vec<Effect> {
        let Some(model) = self.selected_map_model() else {
            self.push_notice("no snapshot loaded — open the Reasoning tab first");
            return Vec::new();
        };
        let levels = crate::reasoning_map::effective_levels(&model);
        if levels.len() <= 1 {
            self.push_notice(format!("{} has no reasoning levels", model.model_id));
            return Vec::new();
        }
        let current = self.prefs.model_defaults.get(&model.model_id).copied();
        let next = match current.and_then(|c| levels.iter().position(|l| *l == c)) {
            Some(idx) => levels[(idx + 1) % levels.len()],
            None => levels[1], // first real level (skip "off")
        };
        self.prefs
            .model_defaults
            .insert(model.model_id.clone(), next);
        self.push_notice(format!(
            "default reasoning for {} → {}",
            model.model_id,
            next.as_str()
        ));
        vec![Effect::SavePrefs]
    }

    fn start_run(&mut self) -> Vec<Effect> {
        let provider = match self.selected_provider() {
            Some(p) => p,
            None => {
                self.push_notice("select a provider first");
                return Vec::new();
            }
        };
        let model = match self.selected_model() {
            Some(m) => m.clone(),
            None => {
                self.push_notice("no models loaded — wait for the list or press 'r'");
                return Vec::new();
            }
        };
        if self.setup.task_input.trim().is_empty() {
            self.push_notice("task is empty — type what to build first");
            return Vec::new();
        }
        let system_prompt = self.system_prompt_for(self.setup.prompt_idx);
        let pricing = self.selected_pricing();
        let reasoning = self.selected_reasoning();
        let task = self.setup.task_input.trim().to_string();
        self.launch_session(provider, model, reasoning, system_prompt, task, pricing)
    }

    fn bulk_start(&mut self) -> Vec<Effect> {
        if self.setup.bulk.is_empty() {
            self.push_notice("no models selected — 'm' + Space to add");
            return Vec::new();
        }
        let missing: Vec<&str> = self
            .setup
            .bulk
            .iter()
            .filter(|(pid, _)| !self.setup.catalog_cache.contains_key(pid))
            .map(|(pid, _)| pid.as_str())
            .collect();
        if !missing.is_empty() {
            self.push_notice(format!(
                "✖ models for {} not loaded — visit the provider first",
                missing.join(", ")
            ));
            return Vec::new();
        }
        if self.setup.task_input.trim().is_empty() {
            self.push_notice("task is empty — type what to build first");
            return Vec::new();
        }
        self.modal = Some(Modal::BulkConfirm);
        Vec::new()
    }

    fn confirm_bulk_start(&mut self) -> Vec<Effect> {
        self.modal = None;
        let specs = self.bulk_specs();
        if specs.is_empty() {
            self.push_notice("no launchable selections");
            return Vec::new();
        }
        let system_prompt = self.system_prompt_for(self.setup.prompt_idx);
        let fallback = self.selected_reasoning();
        let task = self.setup.task_input.trim().to_string();
        let mut effects = Vec::new();
        for spec in specs {
            let provider = match self.registry.get(&spec.provider_id) {
                Some(p) => p,
                None => continue,
            };
            // Per-model default reasoning when set, clamped to the model's
            // supported levels; otherwise the current selection.
            let reasoning = self
                .default_reasoning_for(&spec.model_id)
                .unwrap_or(fallback)
                .clamp_to(spec.model.capabilities.reasoning_levels.as_deref());
            effects.extend(self.launch_session(
                provider,
                spec.model,
                reasoning,
                system_prompt.clone(),
                task.clone(),
                spec.pricing,
            ));
        }
        self.setup.bulk.clear();
        self.setup.multi_select = false;
        self.prefs.remember_task(task);
        self.save_prefs();
        effects.push(Effect::SavePrefs);
        effects
    }

    /// Create (or queue) a run session; returns its launch effect when it
    /// starts immediately.
    fn launch_session(
        &mut self,
        provider: std::sync::Arc<dyn lmhub_core::Provider>,
        model: lmhub_core::ModelInfo,
        reasoning: lmhub_core::ReasoningLevel,
        system_prompt: String,
        task: String,
        pricing: Option<lmhub_core::PricingContext>,
    ) -> Vec<Effect> {
        let cap = self.prefs.max_concurrent_runs.max(1);
        let slot_free = self.runs.running_count() < cap;
        let cancel = CancellationToken::new();
        let id = self.runs.next_id;
        self.runs.next_id += 1;
        let session = RunSession {
            id,
            provider_id: provider.id().to_string(),
            model_id: model.id.clone(),
            reasoning: reasoning.to_string(),
            task: task.clone(),
            status: if slot_free {
                RunSessionStatus::Running
            } else {
                RunSessionStatus::Pending
            },
            started: Instant::now(),
            cancel: Some(cancel.clone()),
            transcript: Transcript::default(),
            tokens: Default::default(),
            tool_ok: 0,
            tool_fail: 0,
            errors: 0,
            warnings: 0,
            pricing: pricing.as_ref().map(|c| c.pricing.clone()),
            finished_line: None,
            final_text: None,
            run_dir: None,
            scroll: 0,
            raw_feed: false,
            delta_count: 0,
            provider,
            model,
            reasoning_level: reasoning,
            system_prompt,
            pricing_ctx: pricing,
        };
        self.runs.runs.push(session);
        self.runs.selected = self.runs.runs.len() - 1;
        self.screen = Screen::Run;
        self.prefs.remember_task(task);
        self.save_prefs();
        let mut effects = vec![Effect::SavePrefs];
        if slot_free {
            effects.push(Effect::LaunchRun { run_id: id });
        } else {
            self.push_notice(format!(
                "concurrency cap reached — {} queued",
                self.runs.runs.len()
            ));
        }
        effects
    }

    fn system_prompt_for(&self, prompt_idx: usize) -> String {
        match self.prompts.get(prompt_idx) {
            Some(p) => lmhub_core::load_prompt(&p.path),
            None => lmhub_core::DEFAULT_SYSTEM_PROMPT.to_string(),
        }
    }

    // ---- palette ----------------------------------------------------------

    /// Currently available palette commands (name + enabled + cmd).
    pub fn palette_commands(&self) -> Vec<(PaletteCmd, String, bool)> {
        vec![
            (
                PaletteCmd::RunSingle,
                PaletteCmd::RunSingle.label().into(),
                self.selected_model().is_some() && !self.setup.task_input.trim().is_empty(),
            ),
            (
                PaletteCmd::BulkRun,
                PaletteCmd::BulkRun.label().into(),
                !self.setup.bulk.is_empty(),
            ),
            (
                PaletteCmd::CancelAllRuns,
                PaletteCmd::CancelAllRuns.label().into(),
                self.runs
                    .runs
                    .iter()
                    .any(|r| r.status != RunSessionStatus::Finished),
            ),
            (
                PaletteCmd::RescanHistory,
                PaletteCmd::RescanHistory.label().into(),
                true,
            ),
            (
                PaletteCmd::OpenOutputDir,
                PaletteCmd::OpenOutputDir.label().into(),
                true,
            ),
            (PaletteCmd::Quit, PaletteCmd::Quit.label().into(), true),
        ]
    }

    fn palette_enter(&mut self) -> Vec<Effect> {
        let Some(Modal::Palette { filter, cursor }) = &self.modal else {
            return Vec::new();
        };
        let (cmd, _, enabled) = match self.filtered_palette(filter.as_str()).get(*cursor) {
            Some(c) => c.clone(),
            None => return Vec::new(),
        };
        if !enabled {
            self.push_notice("that command is not available right now");
            return Vec::new();
        }
        self.modal = None;
        self.run_palette_cmd(cmd)
    }

    pub(crate) fn filtered_palette(&self, filter: &str) -> Vec<(PaletteCmd, String, bool)> {
        let f = filter.trim().to_ascii_lowercase();
        let all = self.palette_commands();
        if f.is_empty() {
            return all;
        }
        all.into_iter()
            .filter(|(_, name, _)| name.to_ascii_lowercase().contains(&f))
            .collect()
    }

    fn palette_run(&mut self, idx: usize) -> Vec<Effect> {
        let Some(Modal::Palette { filter, .. }) = &self.modal else {
            return Vec::new();
        };
        let (cmd, _, _) = match self.filtered_palette(filter.as_str()).get(idx) {
            Some(c) => c.clone(),
            None => return Vec::new(),
        };
        self.modal = None;
        self.run_palette_cmd(cmd)
    }

    fn run_palette_cmd(&mut self, cmd: PaletteCmd) -> Vec<Effect> {
        match cmd {
            PaletteCmd::RunSingle => self.start_run(),
            PaletteCmd::BulkRun => self.bulk_start(),
            PaletteCmd::CancelAllRuns => self.reduce(Action::CancelAllRuns),
            PaletteCmd::RescanHistory => vec![Effect::ScanHistory],
            PaletteCmd::OpenOutputDir => {
                let dir = self.output_base.clone();
                #[cfg(target_os = "macos")]
                let opener = "open";
                #[cfg(not(target_os = "macos"))]
                let opener = "xdg-open";
                match std::process::Command::new(opener).arg(&dir).spawn() {
                    Ok(_) => self.push_notice(format!("opened {}", dir.display())),
                    Err(e) => self.push_notice(format!("✖ could not open {}: {e}", dir.display())),
                }
                Vec::new()
            }
            PaletteCmd::Quit => self.action_quit(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SetupState;

    /// A state with the full provider registry and empty prefs.
    fn test_state() -> (State, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(lmhub_core::AuthStore::load(
            dir.path().join("auth.json"),
        )));
        let (registry, _) =
            lmhub_providers::build_registry(dir.path(), std::sync::Arc::clone(&store));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let state = State::new(
            registry,
            std::sync::Arc::new(lmhub_modelsdev::ModelsDevClient::new(
                dir.path().join("cache"),
                std::time::Duration::from_secs(60),
            )),
            store,
            lmhub_sandbox::SandboxRuntime::Legacy,
            lmhub_core::AppConfig::default(),
            dir.path().join("config.toml"),
            Vec::new(),
            dir.path().join("output"),
            tx,
        );
        (state, dir)
    }

    fn model(id: &str) -> lmhub_core::ModelInfo {
        lmhub_core::ModelInfo {
            id: id.into(),
            name: id.into(),
            ..Default::default()
        }
    }

    /// Fake a loaded catalog for `provider_id` so bulk specs resolve.
    fn seed_catalog(state: &mut State, provider_id: &str, model_ids: &[&str]) {
        let catalog = lmhub_core::ModelCatalog {
            models: model_ids.iter().map(|m| model(m)).collect(),
            source: Some(lmhub_core::ModelListSource::ModelsDev),
            warnings: Vec::new(),
        };
        state.setup.catalog_cache.insert(
            provider_id.to_string(),
            crate::state::CachedCatalog::from_catalog(&catalog, None),
        );
    }

    #[test]
    fn quit_requires_two_presses_with_running_run() {
        let (mut state, _dir) = test_state();
        state.runs.runs.push(RunSession {
            id: 1,
            provider_id: "x".into(),
            model_id: "m".into(),
            reasoning: "off".into(),
            task: "t".into(),
            status: RunSessionStatus::Running,
            started: Instant::now(),
            cancel: Some(CancellationToken::new()),
            transcript: Transcript::default(),
            tokens: Default::default(),
            tool_ok: 0,
            tool_fail: 0,
            errors: 0,
            warnings: 0,
            pricing: None,
            finished_line: None,
            final_text: None,
            run_dir: None,
            scroll: 0,
            raw_feed: false,
            delta_count: 0,
            provider: state.registry.get("openai").unwrap(),
            model: model("gpt-4o"),
            reasoning_level: lmhub_core::ReasoningLevel::Off,
            system_prompt: String::new(),
            pricing_ctx: None,
        });
        assert!(!state.quit);
        let effects = state.reduce(Action::Quit);
        assert!(!state.quit, "first quit only cancels");
        assert!(state.cancel_requested);
        assert!(
            effects.iter().any(|e| matches!(e, Effect::SavePrefs)),
            "prefs persisted on quit"
        );
        state.reduce(Action::ForceQuit);
        assert!(state.quit && state.force_quit);
    }

    #[test]
    fn task_recall_cycles_history() {
        let (mut state, _dir) = test_state();
        state.prefs.task_history = vec!["old".into(), "newer".into()];
        state.reduce(Action::TaskRecall(1));
        assert_eq!(state.setup.task_input, "newer");
        state.reduce(Action::TaskRecall(1));
        assert_eq!(state.setup.task_input, "old");
        // Typing resets recall position.
        state.reduce(Action::TaskChar('!'));
        assert!(state.setup.task_recall_idx.is_none());
    }

    #[test]
    fn bulk_toggle_spans_providers_and_clears() {
        let (mut state, _dir) = test_state();
        seed_catalog(&mut state, "openai", &["gpt-4o", "gpt-4o-mini"]);
        state.setup.models = vec![model("gpt-4o"), model("gpt-4o-mini")];
        state.setup.model_idx = 0;
        state.reduce(Action::ToggleBulk); // auto-enables multi-select
        assert!(state.setup.multi_select);
        assert_eq!(state.setup.bulk.len(), 1);
        state.setup.model_idx = 1;
        state.reduce(Action::ToggleBulk);
        assert_eq!(state.setup.bulk.len(), 2);
        // Switch to another provider: selection survives.
        state.setup.provider_filter = "anthropic".into();
        state.reduce(Action::ClearBulk);
        assert!(state.setup.bulk.is_empty());
    }

    #[test]
    fn bulk_start_queues_beyond_concurrency_cap() {
        let (mut state, _dir) = test_state();
        state.prefs.max_concurrent_runs = 2;
        seed_catalog(&mut state, "openai", &["gpt-4o", "gpt-4o-mini", "gpt-4.1"]);
        state.setup.models = vec![model("gpt-4o"), model("gpt-4o-mini"), model("gpt-4.1")];
        state.setup.task_input = "build a thing".into();
        state.setup.bulk = std::collections::BTreeSet::from([
            ("openai".into(), "gpt-4o".into()),
            ("openai".into(), "gpt-4o-mini".into()),
            ("openai".into(), "gpt-4.1".into()),
        ]);
        let _effects = state.reduce(Action::BulkStart);
        assert!(matches!(state.modal, Some(Modal::BulkConfirm)));
        // Confirm: 2 launch immediately, 1 queues.
        let effects = state.reduce(Action::ConfirmBulkStart);
        let launches: Vec<&Effect> = effects
            .iter()
            .filter(|e| matches!(e, Effect::LaunchRun { .. }))
            .collect();
        assert_eq!(launches.len(), 2, "cap 2 → two launches");
        let statuses: Vec<RunSessionStatus> = state.runs.runs.iter().map(|r| r.status).collect();
        assert_eq!(
            statuses,
            vec![
                RunSessionStatus::Running,
                RunSessionStatus::Running,
                RunSessionStatus::Pending
            ]
        );
        assert!(state.setup.bulk.is_empty(), "bulk cleared after launch");
        // A finish frees a slot → the pending run promotes.
        let effects = state.reduce(Action::UiMsg(crate::UiMsg::RunFinished {
            run_id: 1,
            result: Err("cancelled".into()),
        }));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::LaunchRun { run_id: 3 })),
            "third run promotes when a slot frees"
        );
        assert_eq!(state.runs.runs[2].status, RunSessionStatus::Running);
    }

    #[test]
    fn favorites_toggle_and_persist_flag() {
        let (mut state, _dir) = test_state();
        let provider_id = state.selected_provider().unwrap().id().to_string();
        let effects = state.reduce(Action::ToggleFavorite);
        assert!(state.prefs.favorites.contains(&provider_id));
        assert!(effects.iter().any(|e| matches!(e, Effect::SavePrefs)));
        state.reduce(Action::ToggleFavorite);
        assert!(!state.prefs.favorites.contains(&provider_id));
    }

    #[test]
    fn search_ranks_and_filters() {
        let (mut state, _dir) = test_state();
        state.reduce(Action::SearchProviders("groq".into()));
        let rows = state.provider_rows();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.group.is_none()), "search is flat");
        assert_eq!(rows[0].registry_idx, {
            let all = state.registry.all();
            all.iter().position(|p| p.id() == "groq").unwrap()
        });
        state.reduce(Action::ClearSearch);
        let rows = state.provider_rows();
        assert!(rows.iter().any(|r| r.group.is_some()), "browse has headers");
    }

    #[test]
    fn setup_state_defaults_are_safe() {
        let s = SetupState::default();
        assert_eq!(s.focus, Pane::Providers);
        assert_eq!(s.provider_idx, 0);
        assert!(s.bulk.is_empty());
    }

    #[test]
    fn model_default_reasoning_snaps_on_selection() {
        let (mut state, _dir) = test_state();
        // Two models with distinct reasoning sets.
        let m1 = lmhub_core::ModelInfo {
            id: "claude-3-7-sonnet".into(),
            capabilities: lmhub_core::Capabilities {
                reasoning: true,
                reasoning_levels: Some(vec![
                    lmhub_core::ReasoningLevel::Off,
                    lmhub_core::ReasoningLevel::Low,
                    lmhub_core::ReasoningLevel::High,
                ]),
                ..Default::default()
            },
            ..Default::default()
        };
        state.setup.models = vec![m1.clone()];
        state.setup.model_idx = 0;
        state.setup.reasoning_idx = 0;
        // Pin high as the default.
        state.setup.reasoning_idx = 2;
        let effects = state.reduce(Action::SetModelDefault);
        assert_eq!(
            state.prefs.model_defaults.get("claude-3-7-sonnet"),
            Some(&lmhub_core::ReasoningLevel::High)
        );
        assert!(effects.iter().any(|e| matches!(e, Effect::SavePrefs)));
        // Moving away and back snaps to the default.
        state.setup.reasoning_idx = 0;
        state.snap_reasoning_to_default();
        assert_eq!(state.setup.reasoning_idx, 2);
        let _ = m1;
    }

    #[test]
    fn map_cycle_default_wraps_through_supported_levels() {
        let (mut state, _dir) = test_state();
        state.snapshot_all = Some(std::sync::Arc::new(lmhub_modelsdev::CatalogSnapshot {
            catalog: lmhub_modelsdev::catalog::Catalog {
                providers: std::collections::BTreeMap::from([(
                    "anthropic".into(),
                    lmhub_modelsdev::catalog::ProviderEntry {
                        id: "anthropic".into(),
                        name: "Anthropic".into(),
                        env: vec![],
                        api: None,
                        doc: None,
                        npm: None,
                        models: std::collections::BTreeMap::from([(
                            "claude-3-7-sonnet".into(),
                            serde_json::from_value(serde_json::json!({
                                "id": "claude-3-7-sonnet",
                                "reasoning": true,
                                "reasoning_options": [{ "type": "effort", "values": ["off", "low", "high"] }],
                            }))
                            .unwrap(),
                        )]),
                    },
                )]),
            },
            fetched_at: "t".into(),
            version: "v".into(),
            stale: false,
        }));
        state.map.idx = 0;
        // First d: skip off → low.
        state.reduce(Action::CycleModelDefault);
        assert_eq!(
            state.prefs.model_defaults.get("claude-3-7-sonnet"),
            Some(&lmhub_core::ReasoningLevel::Low)
        );
        // Second d: high; third d: wraps through off → off.
        state.reduce(Action::CycleModelDefault);
        assert_eq!(
            state.prefs.model_defaults.get("claude-3-7-sonnet"),
            Some(&lmhub_core::ReasoningLevel::High)
        );
        state.reduce(Action::CycleModelDefault);
        assert_eq!(
            state.prefs.model_defaults.get("claude-3-7-sonnet"),
            Some(&lmhub_core::ReasoningLevel::Off)
        );
        // And one more lands on low again.
        state.reduce(Action::CycleModelDefault);
        assert_eq!(
            state.prefs.model_defaults.get("claude-3-7-sonnet"),
            Some(&lmhub_core::ReasoningLevel::Low)
        );
    }

    #[test]
    fn bulk_uses_per_model_default_reasoning() {
        let (mut state, _dir) = test_state();
        seed_catalog(&mut state, "openai", &["gpt-4o", "gpt-4o-mini"]);
        state.setup.models = vec![model("gpt-4o"), model("gpt-4o-mini")];
        state.setup.task_input = "build a thing".into();
        state
            .prefs
            .model_defaults
            .insert("gpt-4o-mini".into(), lmhub_core::ReasoningLevel::Low);
        state.setup.bulk = std::collections::BTreeSet::from([
            ("openai".into(), "gpt-4o".into()),
            ("openai".into(), "gpt-4o-mini".into()),
        ]);
        state.reduce(Action::BulkStart);
        state.reduce(Action::ConfirmBulkStart);
        let by_model: std::collections::BTreeMap<String, String> = state
            .runs
            .runs
            .iter()
            .map(|r| (r.model_id.clone(), r.reasoning.clone()))
            .collect();
        // No default → current selection (off); default set → low.
        assert_eq!(by_model["gpt-4o"], "off");
        assert_eq!(by_model["gpt-4o-mini"], "low");
    }
}
