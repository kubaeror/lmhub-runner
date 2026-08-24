//! Draw dispatch: chrome (tabs + footer), per-screen views, modal overlay.
//! Also records the layout cache so mouse clicks can be mapped to panes.

pub mod history;
pub mod palette;
pub mod reasoning;
pub mod run;
pub mod setup;
pub mod shared;

use crate::action::Screen;
use crate::state::State;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Tabs},
    Frame,
};

pub fn draw(f: &mut Frame, state: &mut State) {
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
    state.layout.tab_bar = chunks[0];

    match state.screen {
        Screen::Setup => setup::draw(f, state, chunks[1]),
        Screen::Run => run::draw(f, state, chunks[1]),
        Screen::History => history::draw(f, state, chunks[1]),
        Screen::Reasoning => reasoning::draw(f, state, chunks[1]),
    }

    let footer = Line::from(vec![
        Span::styled(shared::status_line(state), Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(shared::hints(state), Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[2]);

    if let Some(modal) = state.modal.clone() {
        palette::draw(f, state, &modal);
    }
}

/// Map a mouse click to an action using the layout cache from the last draw.
pub fn mouse_action(state: &State, col: u16, row: u16) -> Option<crate::action::Action> {
    use ratatui::layout::Position;
    if state.layout.tab_bar.contains(Position::new(col, row)) {
        let tab_bar = state.layout.tab_bar;
        let n = Screen::ALL.len() as u16;
        let per = tab_bar.width.div_ceil(n).max(1);
        let idx = (col.saturating_sub(tab_bar.x)) / per;
        let idx = (idx as usize).min(Screen::ALL.len() - 1);
        return Some(crate::action::Action::SwitchScreen(Screen::ALL[idx]));
    }
    if state.screen == Screen::Setup {
        for (i, pane) in crate::state::Pane::ORDER.iter().enumerate() {
            if let Some(rect) = state.layout.setup_panes.get(i) {
                if rect.contains(Position::new(col, row)) {
                    return Some(crate::action::Action::FocusPane(*pane));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Pane;
    use ratatui::layout::Rect;

    /// A bare state with the full registry and no prefs — enough for
    /// `mouse_action`, which only inspects `screen` + `layout`.
    fn state_with() -> State {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(lmhub_core::AuthStore::load(
            dir.path().join("auth.json"),
        )));
        let (registry, _) =
            lmhub_providers::build_registry(dir.path(), std::sync::Arc::clone(&store));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        State::new(
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
            Vec::new(),
            dir.path().join("output"),
            tx,
        )
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn setup_mouse_maps_clickable_panes() {
        let mut s = state_with();
        s.screen = Screen::Setup;
        // Simulate the rects recorded by view::setup::draw: providers left,
        // models middle, reasoning/prompts/task stacked on the right. The
        // "Model details" region (right column, top) is NOT a focusable pane.
        let mut panes = [Rect::default(); 5];
        panes[0] = rect(0, 0, 40, 30); // Providers
        panes[1] = rect(40, 0, 40, 30); // Models
        panes[2] = rect(80, 6, 20, 3); // Reasoning levels
        panes[3] = rect(80, 9, 20, 7); // System prompts
        panes[4] = rect(80, 16, 20, 7); // Task prompts
        s.layout.setup_panes = panes;

        assert!(matches!(
            mouse_action(&s, 10, 10),
            Some(crate::action::Action::FocusPane(Pane::Providers))
        ));
        assert!(matches!(
            mouse_action(&s, 50, 10),
            Some(crate::action::Action::FocusPane(Pane::Models))
        ));
        assert!(matches!(
            mouse_action(&s, 85, 7),
            Some(crate::action::Action::FocusPane(Pane::Reasoning))
        ));
        assert!(matches!(
            mouse_action(&s, 85, 12),
            Some(crate::action::Action::FocusPane(Pane::Prompts))
        ));
        assert!(matches!(
            mouse_action(&s, 85, 20),
            Some(crate::action::Action::FocusPane(Pane::Task))
        ));
        // Clicking the read-only "Model details" region focuses nothing.
        assert!(mouse_action(&s, 85, 3).is_none());
    }
}
