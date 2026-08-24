//! Reasoning map: every model across all providers with its supported
//! reasoning levels and per-model defaults.

use crate::reasoning_map::{effective_levels, MapModel};
use crate::state::State;
use crate::view::shared::*;
use crate::view::RenderInfo;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
    Frame,
};

pub fn draw(f: &mut Frame, state: &State, area: Rect, info: &mut RenderInfo) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let rows_all = state.map_rows();
    let cursor = "▏";
    let filter = state.map.filter.as_str();
    let status_text = if state.snapshot_all.is_none() {
        "loading Models.dev snapshot…".to_string()
    } else {
        let providers = rows_all
            .iter()
            .map(|m| m.provider_id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        format!(
            " filter: {filter}{cursor}  — {} models · {providers} providers",
            rows_all.len()
        )
    };
    f.render_widget(
        Paragraph::new(Span::styled(status_text, Style::default().fg(Color::Cyan))),
        rows[0],
    );

    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_item = 0usize;
    let mut last_provider = String::new();
    for (model_pos, model) in rows_all.iter().enumerate() {
        if model.provider_id != last_provider {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("── {} ──", model.provider_id),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ))));
            last_provider = model.provider_id.clone();
        }
        items.push(ListItem::new(model_row(model, state)));
        if model_pos == state.map.idx {
            selected_item = items.len() - 1;
        }
    }
    if rows_all.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            if state.snapshot_all.is_none() {
                "no snapshot yet — F5 to retry"
            } else {
                "no models match the filter"
            },
            Style::default().fg(Color::Magenta),
        ))));
    }

    let total = items.len();
    let list = List::new(items)
        .block(bordered_block(
            " Reasoning map — all models (D = cycle default, F5 = reload) ",
            Style::default().fg(Color::Gray),
        ))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    info.map_list = rows[1];
    let (list_area, bar_area) = scrollbar_area(rows[1], total);
    let mut st = ListState::default().with_selected(Some(selected_item));
    f.render_stateful_widget(list, list_area, &mut st);
    info.offsets.map = st.offset();
    render_scrollbar(f, bar_area, total, st.offset());
}

/// One model row: id, supported levels, ★ on the current default.
fn model_row(model: &MapModel, state: &State) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("  {:<40}", truncate(&model.model_id, 40)),
        Style::default(),
    )];
    let levels = effective_levels(model);
    let default = state.prefs.model_defaults.get(&model.model_id);
    for level in &levels {
        let mut label = level.as_str().to_string();
        if default == Some(level) {
            label.push('★');
        }
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(if default == Some(level) {
                Color::Yellow
            } else {
                Color::Gray
            }),
        ));
    }
    Line::from(spans)
}
