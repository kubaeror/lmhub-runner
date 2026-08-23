//! History screen: table of previous runs; detail opens as a modal.

use crate::state::State;
use crate::view::shared::*;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
    Frame,
};

pub fn draw(f: &mut Frame, state: &State, area: Rect) {
    if state.history.rows.is_empty() {
        f.render_widget(
            Paragraph::new(format!(
                "No previous runs found under {}. Press F5 to rescan.",
                state.output_base.display()
            )),
            area,
        );
        return;
    }

    let rows: Vec<ListItem> = state
        .history
        .rows
        .iter()
        .map(|h| {
            let status_color = match h.status.as_str() {
                "completed" => Color::Green,
                "cancelled" => Color::Yellow,
                _ => Color::Red,
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<10} ", truncate(&h.family, 10))),
                Span::raw(format!("{:<28} ", truncate(&h.model, 28))),
                Span::raw(format!("{:<7} ", h.reasoning)),
                Span::styled(
                    format!("{:<12} ", truncate(&h.status, 12)),
                    Style::default().fg(status_color),
                ),
                Span::raw(format!(
                    "{:>8}ms  tokens {:<10} ",
                    h.duration_ms.unwrap_or(0),
                    h.total_tokens.unwrap_or(0)
                )),
                Span::raw(
                    h.total_usd
                        .map(|c| format!("{c:.6}"))
                        .unwrap_or_else(|| "null".into()),
                ),
            ]))
        })
        .collect();
    let list = List::new(rows)
        .block(bordered_block(
            " Previous runs — family/model/reasoning/status/ms/tokens/$ (Enter = statistics) ",
            Style::default().fg(Color::Gray),
        ))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    let mut st = ListState::default().with_selected(Some(
        state
            .history
            .idx
            .min(state.history.rows.len().saturating_sub(1)),
    ));
    f.render_stateful_widget(list, area, &mut st);
}
