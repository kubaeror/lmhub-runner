//! Reasoning-map reducer: filtering, selection and per-model default
//! reasoning (both the map's `D` cycle and the setup pane's `d` pin land
//! here — they share the same `prefs.model_defaults` persistence).

use crate::action::{Action, Effect};
use crate::state::State;

impl State {
    /// Fold an action owned by the reasoning-map reducer.
    pub(crate) fn reduce_map(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::MapFilter(text) => {
                self.map.filter.set(text);
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
}
