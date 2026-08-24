//! Shared rendering helpers: styles, blocks, truncation, highlights,
//! scrollbars and the footer hint text (derived from `bindings`).

use crate::state::{Pane, State};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Borders, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

pub fn focused_style(current: Pane, wanted: Pane) -> Style {
    if current == wanted {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    }
}

pub fn bordered_block(title: impl Into<String>, style: Style) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .title(Span::styled(title.into(), style))
        .border_style(style)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

/// Render `text` with every match span of `query` highlighted.
pub fn highlighted(text: &str, query: &str, base: Style, highlight: Style) -> Vec<Span<'static>> {
    if query.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let spans = crate::provider_search::highlight_spans(query, text);
    if spans.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut out = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in spans {
        if start > cursor {
            out.push(Span::styled(text[cursor..start].to_string(), base));
        }
        out.push(Span::styled(
            text[start..end].to_string(),
            highlight.add_modifier(Modifier::BOLD),
        ));
        cursor = end;
    }
    if cursor < text.len() {
        out.push(Span::styled(text[cursor..].to_string(), base));
    }
    out
}

/// Footer status text: fresh notice, else the selected run's status.
pub fn status_line(state: &State) -> String {
    if let Some((msg, at)) = &state.notice {
        if at.elapsed().as_secs() < 6 {
            return format!("ⓘ {msg} ");
        }
    }
    if let Some(run) = state.runs.selected_run() {
        return format!(
            "{} {} [{}] — running {}s{} ",
            run.provider_id,
            run.model_id,
            run.reasoning,
            run.started.elapsed().as_secs(),
            if run.status != crate::state::RunSessionStatus::Running {
                " (done)"
            } else {
                ""
            }
        );
    }
    String::new()
}

pub fn hints(state: &State) -> String {
    crate::bindings::hint_text(state.screen)
}

/// Split a list area into (list, scrollbar-column). The scrollbar column is
/// only reserved when the content can actually overflow the viewport.
pub fn scrollbar_area(list_area: Rect, total: usize) -> (Rect, Option<Rect>) {
    if total > list_area.height.saturating_sub(2) as usize {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(list_area);
        (cols[0], Some(cols[1]))
    } else {
        (list_area, None)
    }
}

/// Render a vertical scrollbar into the column returned by [`scrollbar_area`].
pub fn render_scrollbar(f: &mut Frame, bar_area: Option<Rect>, total: usize, offset: usize) {
    let Some(bar_area) = bar_area else {
        return;
    };
    let bar_area = Rect {
        x: bar_area.x,
        y: bar_area.y + 1,
        width: 1,
        height: bar_area.height.saturating_sub(2),
    };
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓")),
        bar_area,
        &mut ScrollbarState::new(total).position(offset),
    );
}

/// Feed-line coloring for the raw event tail.
pub fn feed_style(line: &str) -> Style {
    if line.starts_with('✘') || line.starts_with('✖') || line.starts_with('⛔') {
        Style::default().fg(Color::Red)
    } else if line.starts_with('⚠') || line.starts_with('…') {
        Style::default().fg(Color::Magenta)
    } else if line.starts_with('✔') || line.starts_with('■') {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
    }
}

/// A centered modal rect, clamped to the frame so tiny terminals never get a
/// modal that overflows the screen.
pub fn centered_rect(area: Rect, pct_w: u16, height: u16) -> Rect {
    let w = (area.width.saturating_mul(pct_w)) / 100;
    let w = w.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(4));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: w,
        height,
    }
}
