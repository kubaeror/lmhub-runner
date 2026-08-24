//! Draw dispatch: chrome (tabs + footer), per-screen views, modal overlay.
//!
//! Drawing is **pure** — it never mutates [`State`]. The [`RenderInfo`] it
//! returns captures this frame's geometry, which input handling (mouse
//! clicks) then consumes. This removes the old mutation-during-draw layout
//! cache that could map clicks with stale rectangles.

pub mod history;
pub mod palette;
pub mod reasoning;
pub mod run;
pub mod setup;
pub mod shared;

use crate::action::Screen;
use crate::state::State;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Tabs},
    Frame,
};

/// Geometry of the most recently drawn frame, returned by [`draw`] and
/// consumed by [`mouse_action`]. All-zero by default, so clicks before the
/// first draw map to nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderInfo {
    pub tab_bar: Rect,
    /// Setup panes, indexed by `Pane::ORDER`.
    pub setup_panes: [Rect; 5],
    /// Run screen: `[0]` session list, `[1]` transcript.
    pub run_panes: [Rect; 2],
}

pub fn draw(f: &mut Frame, state: &State) -> RenderInfo {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    let titles: Vec<&str> = Screen::ALL.iter().map(|s| s.title()).collect();
    let idx = Screen::ALL
        .iter()
        .position(|s| *s == state.screen)
        .unwrap_or(0);
    f.render_widget(
        Tabs::new(titles)
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .select(idx),
        chunks[0],
    );

    let mut info = RenderInfo {
        tab_bar: chunks[0],
        ..Default::default()
    };
    match state.screen {
        Screen::Setup => setup::draw(f, state, chunks[1], &mut info.setup_panes),
        Screen::Run => run::draw(f, state, chunks[1], &mut info.run_panes),
        Screen::History => history::draw(f, state, chunks[1]),
        Screen::Reasoning => reasoning::draw(f, state, chunks[1]),
    }

    let footer = Line::from(vec![
        Span::styled(shared::status_line(state), Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(shared::hints(state), Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[2]);

    if let Some(modal) = &state.modal {
        palette::draw(f, state, modal);
    }
    info
}

/// Map a mouse click to an action using the geometry of the last drawn frame.
pub fn mouse_action(
    state: &State,
    info: &RenderInfo,
    col: u16,
    row: u16,
) -> Option<crate::action::Action> {
    use ratatui::layout::Position;
    if info.tab_bar.contains(Position::new(col, row)) {
        let tab_bar = info.tab_bar;
        let n = Screen::ALL.len() as u16;
        let per = tab_bar.width.div_ceil(n).max(1);
        let idx = (col.saturating_sub(tab_bar.x)) / per;
        let idx = (idx as usize).min(Screen::ALL.len() - 1);
        return Some(crate::action::Action::SwitchScreen(Screen::ALL[idx]));
    }
    if state.screen == Screen::Setup {
        for (i, pane) in crate::state::Pane::ORDER.iter().enumerate() {
            if info.setup_panes[i].contains(Position::new(col, row)) {
                return Some(crate::action::Action::FocusPane(*pane));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Pane;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn setup_mouse_maps_clickable_panes() {
        let (state, _dir) = crate::testutil::test_state();
        let mut info = RenderInfo {
            tab_bar: rect(0, 0, 100, 1),
            ..Default::default()
        };
        // Providers left, models middle, reasoning/prompts/task stacked on
        // the right. The "Model details" region (right column, top) is NOT
        // a focusable pane.
        info.setup_panes[0] = rect(0, 1, 40, 30);
        info.setup_panes[1] = rect(40, 1, 40, 30);
        info.setup_panes[2] = rect(80, 7, 20, 3);
        info.setup_panes[3] = rect(80, 10, 20, 7);
        info.setup_panes[4] = rect(80, 17, 20, 7);

        assert!(matches!(
            mouse_action(&state, &info, 10, 10),
            Some(crate::action::Action::FocusPane(Pane::Providers))
        ));
        assert!(matches!(
            mouse_action(&state, &info, 50, 10),
            Some(crate::action::Action::FocusPane(Pane::Models))
        ));
        assert!(matches!(
            mouse_action(&state, &info, 85, 8),
            Some(crate::action::Action::FocusPane(Pane::Reasoning))
        ));
        assert!(matches!(
            mouse_action(&state, &info, 85, 13),
            Some(crate::action::Action::FocusPane(Pane::Prompts))
        ));
        assert!(matches!(
            mouse_action(&state, &info, 85, 20),
            Some(crate::action::Action::FocusPane(Pane::Task))
        ));
        // Clicking the read-only "Model details" region focuses nothing.
        assert!(mouse_action(&state, &info, 85, 3).is_none());
    }

    #[test]
    fn mouse_clicks_tab_bar_switches_screen() {
        let (state, _dir) = crate::testutil::test_state();
        let info = RenderInfo {
            tab_bar: rect(0, 0, 100, 1),
            ..Default::default()
        };
        // 100/4 = 25 per tab: [1]Setup 0-24, [2]Run 25-49, [3]History 50-74,
        // [4]Reasoning 75-99.
        assert!(matches!(
            mouse_action(&state, &info, 30, 0),
            Some(crate::action::Action::SwitchScreen(Screen::Run))
        ));
        assert!(matches!(
            mouse_action(&state, &info, 85, 0),
            Some(crate::action::Action::SwitchScreen(Screen::Reasoning))
        ));
    }
}
