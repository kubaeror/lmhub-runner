//! Setup screen reducer: provider/model/reasoning/prompt/task selection,
//! provider search + favorites, connect + API-key entry, and single/bulk run
//! launch. Pure-ish: state mutations only; spawning happens via effects.

use crate::action::{Action, Effect};
use crate::input::EditField;
use crate::state::{Modal, Pane, RunSession, RunSessionStatus, State};
use crate::transcript::Transcript;
use lmhub_core::StoredCredential;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

impl State {
    /// Fold an action owned by the setup reducer.
    pub(crate) fn reduce_setup(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::CycleFocus(forward) => {
                self.setup.focus = self.setup.focus.cycle(forward);
                Vec::new()
            }
            Action::FocusPane(pane) => {
                self.setup.focus = pane;
                Vec::new()
            }
            Action::MoveSelection(delta) => match self.setup.focus {
                Pane::Providers => self.move_provider(delta),
                Pane::Models => self.move_model(delta),
                Pane::Prompts => self.move_prompt(delta),
                Pane::Task => self.move_task_prompt(delta),
                _ => Vec::new(),
            },
            Action::SearchProviders(text) => {
                self.setup.provider_filter.set(text);
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
            Action::CycleTaskPrompt(delta) => {
                let len = self.task_prompts.len().max(1);
                self.setup.task_prompt_idx =
                    (self.setup.task_prompt_idx as i32 + delta).rem_euclid(len as i32) as usize;
                Vec::new()
            }
            Action::SetDefaultTaskPrompt => self.set_default_task_prompt(),
            Action::StartRun => self.start_run(),
            Action::BulkStart => self.bulk_start(),
            Action::ConfirmBulkStart => self.confirm_bulk_start(),
            Action::RefreshModels(force) => self.request_models(force),

            // ---- key-entry modal --------------------------------------------
            Action::EnterKeyChar(c) => {
                if !c.is_control() {
                    if let Some(Modal::EnterKey { input, .. }) = &mut self.modal {
                        input.insert(c);
                    }
                }
                Vec::new()
            }
            Action::EnterKeyBackspace => {
                if let Some(Modal::EnterKey { input, .. }) = &mut self.modal {
                    input.backspace();
                }
                Vec::new()
            }
            Action::EnterKeyDelete => {
                if let Some(Modal::EnterKey { input, .. }) = &mut self.modal {
                    input.delete();
                }
                Vec::new()
            }
            Action::EnterKeyCursor(delta) => {
                if let Some(Modal::EnterKey { input, .. }) = &mut self.modal {
                    input.move_cursor(delta);
                }
                Vec::new()
            }
            Action::SaveKey => {
                self.save_entered_key();
                Vec::new()
            }
            Action::MouseWheelSetup { pane, dir } => {
                self.setup.focus = pane;
                match pane {
                    Pane::Providers => self.move_provider(dir),
                    Pane::Models => self.move_model(dir),
                    Pane::Prompts => self.move_prompt(dir),
                    Pane::Task => self.move_task_prompt(dir),
                    _ => Vec::new(),
                }
            }
            Action::MouseSelectRow {
                target: crate::action::SelectTarget::Providers,
                idx,
            } => {
                self.setup.focus = Pane::Providers;
                self.select_provider(idx)
            }
            Action::MouseSelectRow {
                target: crate::action::SelectTarget::Models,
                idx,
            } => {
                self.setup.focus = Pane::Models;
                if !self.setup.models.is_empty() {
                    self.setup.model_idx = idx.min(self.setup.models.len() - 1);
                    self.snap_reasoning_to_default();
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    // ---- provider selection / search --------------------------------------

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

    fn move_task_prompt(&mut self, delta: i32) -> Vec<Effect> {
        let len = self.task_prompts.len();
        if len == 0 {
            return Vec::new();
        }
        self.setup.task_prompt_idx =
            ((self.setup.task_prompt_idx as i32 + delta).max(0) as usize).min(len - 1);
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
            // Device flow is long-running: defer to an effect; progress and
            // the final outcome come back as `UiMsg::Notice`.
            self.push_notice("copilot: starting device flow…");
            vec![Effect::RunCopilotFlow]
        } else {
            self.modal = Some(Modal::EnterKey {
                provider_id: id,
                input: EditField::new(),
            });
            Vec::new()
        }
    }

    /// `Enter` inside the key-entry modal: validate + persist the key.
    fn save_entered_key(&mut self) -> bool {
        let Some(Modal::EnterKey { provider_id, input }) = &self.modal else {
            return false;
        };
        let id = provider_id.clone();
        let key = input.as_str().trim().to_string();
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

    // ---- bulk --------------------------------------------------------------

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

    // ---- defaults ----------------------------------------------------------

    fn set_default_task_prompt(&mut self) -> Vec<Effect> {
        let Some(p) = self.task_prompts.get(self.setup.task_prompt_idx) else {
            return Vec::new();
        };
        self.config.default_task_prompt = Some(p.name.clone());
        match self.config.save(&self.config_path) {
            Ok(()) => self.push_notice(format!("default task prompt → {}", p.name)),
            Err(e) => self.push_notice(format!("✖ could not save config: {e}")),
        }
        self.capture_prefs();
        vec![Effect::SavePrefs]
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
        self.capture_prefs();
        vec![Effect::SavePrefs]
    }

    // ---- runs --------------------------------------------------------------

    /// Start the current single selection; also invoked by the palette.
    pub(crate) fn start_run(&mut self) -> Vec<Effect> {
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
        let task = match self.selected_task_prompt() {
            Some(t) => t,
            None => {
                self.push_notice("no task prompt available — add one to prompts/task-prompts/");
                return Vec::new();
            }
        };
        let system_prompt = self.system_prompt_for(self.setup.prompt_idx);
        let pricing = self.selected_pricing();
        let reasoning = self.selected_reasoning();
        self.launch_session(provider, model, reasoning, system_prompt, task, pricing)
    }

    /// Open the bulk-launch confirmation; also invoked by the palette.
    pub(crate) fn bulk_start(&mut self) -> Vec<Effect> {
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
        if self.selected_task_prompt().is_none() {
            self.push_notice("no task prompt available — add one to prompts/task-prompts/");
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
        let Some(task) = self.selected_task_prompt() else {
            self.push_notice("no task prompt available — add one to prompts/task-prompts/");
            return Vec::new();
        };
        let system_prompt = self.system_prompt_for(self.setup.prompt_idx);
        let fallback = self.selected_reasoning();
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
        self.capture_prefs();
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
        self.screen = crate::action::Screen::Run;
        self.capture_prefs();
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

    /// Resolve the selected task prompt to its text (the first user message).
    /// Falls back to the built-in task prompt when the list is empty.
    fn selected_task_prompt(&self) -> Option<String> {
        match self.task_prompts.get(self.setup.task_prompt_idx) {
            Some(p) => Some(lmhub_core::load_task_prompt(&p.path)),
            None => Some(lmhub_core::DEFAULT_TASK_PROMPT.to_string()),
        }
    }
}
