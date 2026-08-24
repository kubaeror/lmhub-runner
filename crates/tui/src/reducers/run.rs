//! Run screen reducer: session selection, transcript scrolling, cancel,
//! rerun, raw-feed toggle and the run-detail modal.

use crate::action::{Action, Effect};
use crate::state::{Modal, RunSessionStatus, State};
use crate::transcript::Transcript;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

impl State {
    /// Fold an action owned by the run reducer.
    pub(crate) fn reduce_run(&mut self, action: Action) -> Vec<Effect> {
        match action {
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
            Action::CancelAllRuns => self.cancel_all_runs(),
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
            Action::MouseSelectRow {
                target: crate::action::SelectTarget::Sessions,
                idx,
            } => {
                if !self.runs.runs.is_empty() {
                    self.runs.selected = idx.min(self.runs.runs.len() - 1);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Cancel every running run (queued ones are marked finished).
    pub(crate) fn cancel_all_runs(&mut self) -> Vec<Effect> {
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
}
