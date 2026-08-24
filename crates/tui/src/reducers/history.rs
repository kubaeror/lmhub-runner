//! History screen reducer: selection, rescan and the statistics detail modal.

use crate::action::{Action, Effect};
use crate::history;
use crate::state::{Modal, State};

impl State {
    /// Fold an action owned by the history reducer.
    pub(crate) fn reduce_history(&mut self, action: Action) -> Vec<Effect> {
        match action {
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
            _ => Vec::new(),
        }
    }
}
