//! Modal overlays: command palette, bulk confirmation, history/run detail,
//! API-key entry.

use crate::reduce::PaletteCmd;
use crate::state::{Modal, State};
use crate::view::shared::*;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

pub fn draw(f: &mut Frame, state: &mut State, modal: &Modal) {
    match modal {
        Modal::EnterKey { provider_id, input } => {
            draw_key_entry(
                f,
                provider_id,
                input,
                state.setup.focus == crate::state::Pane::Providers,
            );
        }
        Modal::Palette { filter, cursor } => draw_palette(f, state, filter, *cursor),
        Modal::BulkConfirm => draw_bulk_confirm(f, state),
        Modal::HistoryDetail(text) => draw_detail(f, text, " statistics (Esc closes) "),
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
    if !has_prices {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "pricing unknown for these routes — costs will be null",
            Style::default().fg(Color::Magenta),
        )));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_detail(f: &mut Frame, text: &str, title: &str) {
    let area = centered_rect(f.area(), 70, 16);
    let inner = modal_frame(f, area, title.into());
    f.render_widget(
        Paragraph::new(text.to_string()).wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_run_detail(f: &mut Frame, state: &State, run_id: u64) {
    let Some(run) = state.runs.find(run_id) else {
        return;
    };
    let area = centered_rect(f.area(), 70, 16);
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
