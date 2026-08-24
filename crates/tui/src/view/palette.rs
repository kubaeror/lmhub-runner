//! Modal overlays: command palette, bulk confirmation, history/run detail,
//! API-key entry.

use crate::reducers::PaletteCmd;
use crate::state::{Modal, State};
use crate::view::shared::*;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

pub fn draw(f: &mut Frame, state: &State, modal: &Modal) {
    match modal {
        Modal::EnterKey { provider_id, input } => {
            draw_key_entry(
                f,
                provider_id,
                input.as_str(),
                state.setup.focus == crate::state::Pane::Providers,
            );
        }
        Modal::Palette { filter, cursor } => draw_palette(f, state, filter.as_str(), *cursor),
        Modal::Help => draw_help(f, state),
        Modal::BulkConfirm => draw_bulk_confirm(f, state),
        Modal::HistoryDetail { text, scroll } => {
            draw_detail(f, text, *scroll, " statistics (Esc closes, ↑/↓ scroll) ")
        }
        Modal::RunDetail { run_id } => draw_run_detail(f, state, *run_id),
    }
}

fn modal_frame(f: &mut Frame, area: Rect, title: String) -> Rect {
    f.render_widget(Clear, area);
    let block = ratatui::widgets::Block::new()
        .borders(ratatui::widgets::Borders::ALL)
        .title(Span::styled(title, Style::default().fg(Color::Yellow)))
        .border_style(Style::default().fg(Color::Yellow));
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    f.render_widget(block, area);
    inner
}

fn draw_key_entry(f: &mut Frame, provider_id: &str, input: &str, _focused: bool) {
    let area = centered_rect(f.area(), 60, 5);
    let inner = modal_frame(
        f,
        area,
        format!(" API key for {provider_id} (Enter=save, Esc=cancel) "),
    );
    let shown = format!("{}|", "•".repeat(input.chars().count()));
    f.render_widget(Paragraph::new(shown), inner);
}

fn draw_palette(f: &mut Frame, state: &State, filter: &str, cursor: usize) {
    let area = centered_rect(f.area(), 55, 12);
    let inner = modal_frame(f, area, " Commands (type to filter, Esc closes) ".into());

    let rows = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Min(1),
        ])
        .split(inner);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("filter: {filter}▏"),
            Style::default().fg(Color::Cyan),
        )),
        rows[0],
    );

    let commands = state.filtered_palette(filter);
    let items: Vec<ListItem> = commands
        .iter()
        .map(|(cmd, name, enabled)| {
            let style = if *enabled {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ListItem::new(Line::styled(format!("{} {}", cmd_icon(*cmd), name), style))
        })
        .collect();
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let mut st =
        ListState::default().with_selected(Some(cursor.min(commands.len().saturating_sub(1))));
    f.render_stateful_widget(list, rows[1], &mut st);
}

fn cmd_icon(cmd: PaletteCmd) -> &'static str {
    match cmd {
        PaletteCmd::RunSingle => "▶",
        PaletteCmd::BulkRun => "☑",
        PaletteCmd::CancelAllRuns => "✖",
        PaletteCmd::RescanHistory => "↻",
        PaletteCmd::OpenOutputDir => "»",
        PaletteCmd::Quit => "✕",
    }
}

fn draw_bulk_confirm(f: &mut Frame, state: &State) {
    let specs = state.bulk_specs();
    let area = centered_rect(f.area(), 55, 14);
    let inner = modal_frame(
        f,
        area,
        format!(
            " Bulk launch — {} run{} (y/Enter = launch) ",
            specs.len(),
            if specs.len() == 1 { "" } else { "s" }
        ),
    );

    let mut lines: Vec<Line> = Vec::new();
    let mut last_provider = String::new();
    let mut has_prices = false;
    for spec in &specs {
        if spec.provider_id != last_provider {
            lines.push(Line::from(Span::styled(
                format!("─ {} ", spec.provider_id),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )));
            last_provider = spec.provider_id.clone();
        }
        let mut spans = vec![Span::raw(format!("  • {}", spec.model.id))];
        // Show the reasoning this run will actually get (pinned default or
        // the chosen level, clamped to the model) — same logic as launch.
        spans.push(Span::styled(
            format!("  [{}]", state.bulk_reasoning_for(spec).as_str()),
            Style::default().fg(Color::Yellow),
        ));
        if let Some(pc) = &spec.pricing {
            has_prices = true;
            spans.push(Span::styled(
                format!(
                    "  (${}/1M in · ${}/1M out)",
                    pc.pricing.input_per_million_usd, pc.pricing.output_per_million_usd
                ),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(
        format!(
            "fallback reasoning: {} (set per model with ↑/↓)",
            state.setup.reasoning.as_str()
        ),
        Style::default().fg(Color::DarkGray),
    )));
    if !has_prices {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "pricing unknown for these routes — costs will be null",
            Style::default().fg(Color::Magenta),
        )));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Keybinding help overlay: global bindings + the current screen's.
fn draw_help(f: &mut Frame, state: &State) {
    let area = centered_rect(f.area(), 70, 20);
    let inner = modal_frame(f, area, " Keybindings (? closes) ".into());
    let help_lines = crate::bindings::help_lines(state.screen);
    let lines: Vec<Line> = help_lines
        .iter()
        .map(|l| {
            if l.starts_with("──") {
                Line::styled(
                    l.as_str(),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Line::raw(l.as_str())
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// Scrollable pretty-printed text modal (statistics detail).
fn draw_detail(f: &mut Frame, text: &str, scroll: usize, title: &str) {
    let area = centered_rect(f.area(), 70, 16);
    let inner = modal_frame(f, area, title.into());
    let all_lines: Vec<&str> = text.lines().collect();
    let max = all_lines.len().saturating_sub(1);
    let start = scroll.min(max);
    let end = (start + inner.height as usize).min(all_lines.len());
    let lines: Vec<Line> = all_lines[start..end]
        .iter()
        .map(|l| Line::raw(*l))
        .collect();
    let bar = Rect {
        x: area.x + area.width.saturating_sub(1),
        y: area.y,
        width: 1,
        height: area.height,
    };
    f.render_widget(Paragraph::new(lines), inner);
    render_scrollbar(f, Some(bar), all_lines.len(), start);
}

fn draw_run_detail(f: &mut Frame, state: &State, run_id: u64) {
    let Some(run) = state.runs.find(run_id) else {
        return;
    };
    let area = centered_rect(f.area(), 70, 18);
    let inner = modal_frame(
        f,
        area,
        format!(" run #{} — {} (Esc closes) ", run.id, run.model_id),
    );
    let mut lines: Vec<Line> = vec![
        Line::from(format!("provider : {}", run.provider_id)),
        Line::from(format!("model    : {}", run.model_id)),
        Line::from(format!("reasoning: {}", run.reasoning)),
        Line::from(format!("task     : {}", truncate(&run.task, 120))),
        Line::from(""),
    ];
    if let Some(done) = &run.finished_line {
        lines.push(Line::from(Span::styled(
            done.clone(),
            Style::default().fg(if done.contains("failure") {
                Color::Red
            } else {
                Color::Green
            }),
        )));
        lines.push(Line::from(""));
    }
    if let Some(dir) = &run.run_dir {
        lines.push(Line::from(format!("run dir  : {}", dir.display())));
    }
    if let Some(text) = &run.final_text {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "final answer:",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for chunk in text.chars().take(1_200).collect::<Vec<_>>().chunks(100) {
            lines.push(Line::from(String::from_iter(chunk)));
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
