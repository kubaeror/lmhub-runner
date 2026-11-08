//! Reasoning-map reducer: filtering, selection and per-model default
//! reasoning (both the map's `D` cycle and the setup pane's `d` pin land
//! here — they share the same `prefs.model_defaults` persistence).

use crate::action::{Action, Effect};
use crate::state::State;

impl State {
    /// Fold an action owned by the reasoning-map reducer.
    pub(crate) fn reduce_map(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::MapClear => {
                self.map.filter.clear();
                self.refresh_map_rows();
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
                self.refresh_map_rows();
                vec![Effect::LoadSnapshot]
            }
            Action::MouseSelectRow {
                target: crate::action::SelectTarget::Map,
                idx,
            } => {
                if !self.map_rows().is_empty() {
                    self.map.idx = idx.min(self.map_rows().len() - 1);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// `d` in the Setup Reasoning pane: pin the current level as the
    /// current model's default (persisted, used on future selections).
    fn set_model_default(&mut self) -> Vec<Effect> {
        let Some(pid) = self.selected_provider_id() else {
            self.push_notice("select a provider first");
            return Vec::new();
        };
        let Some(model_id) = self.selected_model().map(|m| m.id.clone()) else {
            self.push_notice("select a model first");
            return Vec::new();
        };
        let level = self.selected_reasoning();
        self.set_model_default_reasoning(&pid, &model_id, level);
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
        let current = self.default_reasoning_for(&model.provider_id, &model.model_id);
        let next = match current.and_then(|c| levels.iter().position(|l| *l == c)) {
            Some(idx) => levels[(idx + 1) % levels.len()],
            // No pinned default: continue from the user's current level when
            // the model supports it, else start at the first supported one
            // (never a blind `levels[1]`).
            None => match levels.iter().position(|l| *l == self.setup.reasoning) {
                Some(idx) => levels[(idx + 1) % levels.len()],
                None => levels
                    .first()
                    .copied()
                    .unwrap_or(lmhub_core::ReasoningLevel::Off),
            },
        };
        self.set_model_default_reasoning(&model.provider_id, &model.model_id, next);
        self.push_notice(format!(
            "default reasoning for {} → {}",
            model.model_id,
            next.as_str()
        ));
        vec![Effect::SavePrefs]
    }
}
