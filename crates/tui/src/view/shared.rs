//! Shared rendering helpers: styles, blocks, truncation, highlights.

use crate::state::{Pane, State};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Borders},
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

pub fn hints(state: &State) -> &'static str {
    match state.screen {
        // On filter screens `q` types into the filter — quit is Ctrl-C.
        crate::action::Screen::Setup => {
            "type=search providers  ↑/↓ select  Ctrl-Enter run  ←/→ pane  m multi  Space bulk  F favorite  x bulk-run  F5 models  d set default  : palette  Ctrl-C quit"
        }
        crate::action::Screen::Run => {
            "[/] session  ↑/↓ scroll  c cancel  C all  R rerun  v raw feed  Enter detail  : palette  q quit"
        }
        crate::action::Screen::History => "↑/↓ select  Enter detail  F5 rescan  : palette  q quit",
        crate::action::Screen::Reasoning => {
            "type=filter  ↑/↓ select  D cycle default (★)  Esc clear  F5 reload  : palette  Ctrl-C quit"
        }
    }
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

pub fn centered_rect(area: Rect, pct_w: u16, height: u16) -> Rect {
    let w = (area.width.saturating_mul(pct_w)) / 100;
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + 3,
        width: w,
        height,
    }
}
