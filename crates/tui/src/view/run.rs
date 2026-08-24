//! Run screen: session list, live transcript (structured or raw feed),
//! per-session stats bar.

use crate::pricing;
use crate::state::{RunSession, RunSessionStatus, State};
use crate::view::shared::*;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
    Frame,
};

pub fn draw(f: &mut Frame, state: &mut State, area: Rect) {
    if state.runs.runs.is_empty() {
        f.render_widget(
            Paragraph::new(
                "No runs yet. Configure one in Setup and press Ctrl+Enter on Task prompts.",
            ),
            area,
        );
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(1)])
        .split(chunks[0]);

    state.layout.run_panes[0] = cols[0];
    state.layout.run_panes[1] = cols[1];

    draw_sessions(f, state, cols[0]);
    draw_transcript(f, state, cols[1]);
    draw_stats(f, state, chunks[1]);
}

fn draw_sessions(f: &mut Frame, state: &State, area: Rect) {
    let items: Vec<ListItem> = state
        .runs
        .runs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let marker = if i == state.runs.selected {
                "▶ "
            } else {
                "  "
            };
            let (status_color, status) = match r.status {
                RunSessionStatus::Pending => (Color::Yellow, "queued".to_string()),
                RunSessionStatus::Running => {
                    (Color::Cyan, format!("{}s", r.started.elapsed().as_secs()))
                }
                RunSessionStatus::Finished => (
                    if r.finished_line
                        .as_deref()
                        .map(|l| l.contains("failure"))
                        .unwrap_or(false)
                    {
                        Color::Red
                    } else {
                        Color::Green
                    },
                    "done".to_string(),
                ),
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("#{} {}", r.id, truncate(&r.model_id, 14)),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" [{}] ", r.reasoning),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(status, Style::default().fg(status_color)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(bordered_block(
            " Sessions ([/]) ",
            Style::default().fg(Color::Gray),
        ))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    let mut st = ListState::default().with_selected(Some(state.runs.selected));
    f.render_stateful_widget(list, area, &mut st);
}

fn draw_transcript(f: &mut Frame, state: &State, area: Rect) {
    let Some(run) = state.runs.selected_run() else {
        return;
    };
    let max_w = area.width.saturating_sub(4) as usize;
    let all_lines = transcript_lines(run, max_w.max(20));

    let max_lines = area.height.saturating_sub(2) as usize;
    let total = all_lines.len();
    let back = run.scroll.min(total);
    let end = total - back;
    let start = end.saturating_sub(max_lines);
    let visible: Vec<Line> = all_lines[start..end].to_vec();

    let title = if run.raw_feed {
        format!(" run #{} — raw events ", run.id)
    } else {
        format!(" run #{} — transcript (v=raw) ", run.id)
    };
    let block = bordered_block(title, Style::default().fg(Color::Gray));
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    f.render_widget(Paragraph::new(visible), inner);
    f.render_widget(block, area);
}

/// All transcript lines (structured turns or raw feed), oldest first.
fn transcript_lines(run: &RunSession, max_w: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    if run.raw_feed {
        for l in &run.transcript.feed {
            lines.push(Line::styled(l.clone(), feed_style(l)));
        }
        return lines;
    }
    for turn in &run.transcript.turns {
        let mut meta = String::new();
        if turn.duration_ms > 0 {
            meta.push_str(&format!(" · {} ms", turn.duration_ms));
        }
        if !turn.stop_reason.is_empty() {
            meta.push_str(&format!(" · stop {}", turn.stop_reason));
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("── turn {} ", turn.number),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(meta, Style::default().fg(Color::DarkGray)),
        ]));
        let text: String = turn.llm_text.chars().take(4_000).collect();
        for chunk in wrap_chunks(&text, max_w) {
            lines.push(Line::styled(chunk, Style::default()));
        }
        for tc in &turn.tool_calls {
            let ok = tc.status == "success";
            let mut spans = vec![Span::styled(
                format!("{} {} ", if ok { "✔" } else { "✘" }, tc.name),
                Style::default().fg(if ok { Color::Green } else { Color::Red }),
            )];
            spans.push(Span::styled(
                format!("({} ms)", tc.duration_ms),
                Style::default().fg(Color::DarkGray),
            ));
            if let Some(err) = &tc.error {
                spans.push(Span::styled(
                    format!(" — {}", truncate(err, 120)),
                    Style::default().fg(Color::Red),
                ));
            }
            lines.push(Line::from(spans));
        }
    }
    // Live streaming tail of the current turn.
    if run.status == RunSessionStatus::Running {
        if let Some(turn) = run.transcript.live_turn() {
            if !turn.llm_text.is_empty() {
                lines.push(Line::from(vec![Span::styled(
                    format!("streaming… ({} deltas)", run.delta_count),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )]));
                let tail: String = turn
                    .llm_text
                    .chars()
                    .rev()
                    .take(max_w * 3)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                for chunk in wrap_chunks(&tail, max_w) {
                    lines.push(Line::styled(chunk, Style::default().fg(Color::Cyan)));
                }
            }
        }
    }
    lines
}

/// Simple character-based wrapping (UTF-8 safe).
fn wrap_chunks(text: &str, max_w: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let max_w = max_w.max(1);
    text.chars()
        .collect::<Vec<_>>()
        .chunks(max_w)
        .map(|c| c.iter().collect())
        .collect()
}

fn draw_stats(f: &mut Frame, state: &State, area: Rect) {
    let Some(run) = state.runs.selected_run() else {
        return;
    };
    let u = &run.tokens;
    let cost = run
        .pricing
        .as_ref()
        .map(|p| format!("{:.6} USD", pricing::estimate_cost(p, u)))
        .unwrap_or_else(|| "null".into());
    let hit = pricing::cache_hit_ratio(u)
        .map(|r| format!("{r:.4}"))
        .unwrap_or_else(|| "null".into());
    let row0 = Line::from(vec![
        Span::styled(
            format!(
                "{} {} [{}] · {}s · tok in {} out {} · cache-hit {} · tools ✔{} ✘{} · err {} warn {} · est {}",
                run.provider_id,
                run.model_id,
                run.reasoning,
                run.started.elapsed().as_secs(),
                u.input_tokens,
                u.output_tokens,
                hit,
                run.tool_ok,
                run.tool_fail,
                run.errors,
                run.warnings,
                cost,
            ),
            Style::default().fg(Color::Cyan),
        ),
    ]);
    let row1: Line = if let Some(done) = &run.finished_line {
        Line::styled(
            done.clone(),
            Style::default().fg(if done.contains("failure") {
                Color::Red
            } else {
                Color::Green
            }),
        )
    } else if run.status == RunSessionStatus::Pending {
        Line::styled(
            format!("queued — {} will start when a slot frees", run.model_id),
            Style::default().fg(Color::Yellow),
        )
    } else {
        Line::styled(
            format!("streaming… ({} deltas)", run.delta_count),
            Style::default().fg(Color::Cyan),
        )
    };
    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    f.render_widget(Paragraph::new(vec![row0, row1]), inner);
}
