//! Draw dispatch: chrome (tabs + footer), per-screen views, modal overlay.
//! Also records the layout cache so mouse clicks can be mapped to panes.

pub mod history;
pub mod palette;
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
