//! Global / cross-screen reducer: quit, screen switching, the transient
//! notice line, UI messages from background tasks, bracketed paste, and the
//! command palette. Anything async or blocking becomes an [`Effect`] whose
//! result re-enters as `Action::UiMsg` — this reducer never spawns itself.

use crate::action::{Action, Effect, Screen};
use crate::state::{fold_run_event, CachedCatalog, Modal, Pane, RunSessionStatus, State};
use std::time::Instant;

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
    /// Fold an action owned by the global reducer.
    pub(crate) fn reduce_global(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::Quit => self.action_quit(),
            Action::ForceQuit => {
                self.quit = true;
                self.force_quit = true;
                self.capture_prefs();
                vec![Effect::SavePrefs]
            }
            Action::SwitchScreen(screen) => self.switch_screen(screen),
            Action::OpenPalette => {
                self.modal = Some(Modal::Palette {
                    filter: Default::default(),
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
            Action::Paste(text) => self.paste_text(text),
            Action::UiMsg(msg) => self.reduce_ui_msg(msg),

            // ---- palette ---------------------------------------------------
            Action::PaletteChar(c) => {
                if c.is_control() {
                    return Vec::new();
                }
                if let Some(Modal::Palette { filter, .. }) = &mut self.modal {
                    filter.insert(c);
                }
                Vec::new()
            }
            Action::PaletteBackspace => {
                if let Some(Modal::Palette { filter, .. }) = &mut self.modal {
                    filter.backspace();
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
            _ => Vec::new(),
        }
    }

    fn switch_screen(&mut self, screen: Screen) -> Vec<Effect> {
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
        self.capture_prefs();
        vec![Effect::SavePrefs]
    }

    /// Snapshot current selections into prefs (in-memory). The event loop
    /// persists them via [`Effect::SavePrefs`] — a single disk write per
    /// mutating action, never a write from inside the reducer.
    pub(crate) fn capture_prefs(&mut self) {
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
        if let Some(p) = self.task_prompts.get(self.setup.task_prompt_idx) {
            self.prefs.last_task_prompt = Some(p.name.clone());
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
                let cache = CachedCatalog::from_catalog(&catalog, snapshot.clone());
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
            pending.cancel = Some(tokio_util::sync::CancellationToken::new());
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

    // ---- paste ------------------------------------------------------------

    /// Paste (bracketed-paste text) into the focused input field.
    ///
    /// Line endings are normalized to `\n`. Single-line fields (modals,
    /// search filters) get every control character stripped.
    fn paste_text(&mut self, raw: String) -> Vec<Effect> {
        let text: String = raw
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        if let Some(Modal::EnterKey { input, .. }) = &mut self.modal {
            input.paste(&text);
            return Vec::new();
        }
        if let Some(Modal::Palette { filter, .. }) = &mut self.modal {
            filter.paste(&text);
            return Vec::new();
        }
        match self.screen {
            Screen::Setup if self.setup.focus == Pane::Providers => {
                self.setup.provider_filter.paste(&text);
            }
            Screen::Reasoning => {
                self.map.filter.paste(&text);
            }
            _ => {}
        }
        Vec::new()
    }

    // ---- palette ----------------------------------------------------------

    /// Currently available palette commands (name + enabled + cmd).
    pub fn palette_commands(&self) -> Vec<(PaletteCmd, String, bool)> {
        vec![
            (
                PaletteCmd::RunSingle,
                PaletteCmd::RunSingle.label().into(),
                self.selected_model().is_some() && !self.task_prompts.is_empty(),
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
            PaletteCmd::CancelAllRuns => self.cancel_all_runs(),
            PaletteCmd::RescanHistory => vec![Effect::ScanHistory],
            PaletteCmd::OpenOutputDir => vec![Effect::OpenOutputDir(self.output_base.clone())],
            PaletteCmd::Quit => self.action_quit(),
        }
    }
}
