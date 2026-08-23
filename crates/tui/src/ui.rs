//! Ratatui rendering for all three tabs.

use crate::app::{App, Focus, Mode, Tab};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame,
};

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    let titles: Vec<&str> = Tab::ALL.iter().map(|t| t.title()).collect();
    let idx = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
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

    match app.tab {
        Tab::Setup => draw_setup(f, app, chunks[1]),
        Tab::Run => draw_run(f, app, chunks[1]),
        Tab::History => draw_history(f, app, chunks[1]),
    }

    let hints = match app.tab {
        Tab::Setup => "type=filter providers  ↑/↓ select  Enter connect/run/models  ←/→ pane  F5 models  d default prompt  Tab tabs  q quit",
        Tab::Run => "c cancel run  ↑/↓ scroll feed  Tab tabs (new run: Setup → Task → Enter)",
        Tab::History => "↑/↓ select  Enter detail  Esc close  F5 rescan  Tab tabs  q quit",
    };
    let status = if let Some((msg, at)) = &app.notice {
        if at.elapsed().as_secs() < 6 {
            format!("ⓘ {msg} ")
        } else if let Some(run) = &app.run {
            run_status_line(run)
        } else {
            String::new()
        }
    } else if let Some(run) = &app.run {
        run_status_line(run)
    } else {
        String::new()
    };
    let footer = Line::from(vec![
        Span::styled(status, Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(hints, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[2]);

    // Key-entry overlay
    if let Mode::EnterKey { provider_id } = &app.mode {
        let area = centered_rect(f.area(), 60, 5);
        f.render_widget(ratatui::widgets::Clear, area);
        let block = Block::new()
            .borders(Borders::ALL)
            .title(Span::styled(
                format!(" API key for {provider_id} (Enter=save, Esc=cancel) "),
                Style::default().fg(Color::Yellow),
            ))
            .border_style(Style::default().fg(Color::Yellow));
        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        let shown = format!("{}|", "•".repeat(app.key_input.chars().count()));
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(shown), inner);
    }
}

fn centered_rect(area: Rect, pct_w: u16, height: u16) -> Rect {
    let w = (area.width.saturating_mul(pct_w)) / 100;
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + 3,
        width: w,
        height,
    }
}

fn draw_providers_pane(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    // filter line
    let cursor = if app.focus == Focus::Providers {
        "|"
    } else {
        ""
    };
    let filter_line = Line::from(Span::styled(
        format!("/{}{cursor}", app.provider_filter),
        Style::default().fg(Color::Cyan),
    ));
    f.render_widget(Paragraph::new(filter_line), rows[0]);

    let indices = app.filtered_indices();
    let items: Vec<ListItem> = indices
        .iter()
        .filter_map(|i| app.registry.all().get(*i).map(|p| (*i, p)))
        .map(|(idx, p)| {
            let badge = app.provider_badge(idx);
            let color = match badge {
                "[key ok]" => Color::Green,
                "[local]" => Color::Blue,
                _ => Color::Red,
            };
            ListItem::new(Line::from(vec![
                Span::raw(p.display_name().to_string()),
                Span::styled(format!(" {badge}"), Style::default().fg(color)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::new()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " Providers ",
                    focused_style(app.focus, Focus::Providers),
                ))
                .border_style(focused_style(app.focus, Focus::Providers)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    let count = indices.len();
    let mut st =
        ListState::default().with_selected(Some(app.provider_idx.min(count.saturating_sub(1))));
    f.render_stateful_widget(list, rows[1], &mut st);
}

fn focused_style(current: Focus, wanted: Focus) -> Style {
    if current == wanted {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    }
}

fn draw_setup(f: &mut Frame, app: &mut App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    // ---- left: providers + models -----------------------------------------
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(1)])
        .split(cols[0]);

    draw_providers_pane(f, app, left[0]);

    let models_title = match (app.models_loading, app.model_source) {
        (true, _) => " Models — loading… ".to_string(),
        (false, Some(src)) => format!(" Models [{src}] "),
        (false, None) => " Models [no list available] ".to_string(),
    };
    let model_items: Vec<ListItem> = app
        .models
        .iter()
        .map(|m| {
            let mut label = m.name.clone();
            if !m.capabilities.reasoning {
                label.push_str("  ·noreason");
            }
            if !m.capabilities.tool_call {
                label.push_str("  ·notools");
            }
            ListItem::new(label)
        })
        .collect();
    let models_list = List::new(model_items)
        .block(
            Block::new()
                .borders(Borders::ALL)
                .title(Span::styled(
                    models_title,
                    focused_style(app.focus, Focus::Models),
                ))
                .border_style(focused_style(app.focus, Focus::Models)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    let mut mst = ListState::default()
        .with_selected(Some(app.model_idx.min(app.models.len().saturating_sub(1))));
    f.render_stateful_widget(models_list, left[1], &mut mst);

    // ---- right: details + reasoning + prompts + task ----------------------
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(11),
            Constraint::Length(4),
            Constraint::Length(8),
            Constraint::Min(1),
        ])
        .split(cols[1]);

    draw_model_details(f, app, right[0]);
    draw_reasoning(f, app, right[1]);
    draw_prompts(f, app, right[2]);

    let task_block = Block::new()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Task (Enter = RUN) ",
            focused_style(app.focus, Focus::Task),
        ))
        .border_style(focused_style(app.focus, Focus::Task));
    let inner = Rect {
        x: right[3].x + 1,
        y: right[3].y + 1,
        width: right[3].width.saturating_sub(2),
        height: right[3].height.saturating_sub(2),
    };
    let cursor = if app.focus == Focus::Task { "|" } else { "" };
    let shown: String = {
        let w = inner.width.saturating_sub(3) as usize;
        let s = format!("{}{cursor}", app.task_input);
        s.chars()
            .rev()
            .take(w.max(1))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };
    f.render_widget(Paragraph::new(shown).wrap(Wrap { trim: false }), inner);
    f.render_widget(task_block, right[3]);
}

fn draw_model_details(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    match app.selected_model() {
        Some(m) => {
            lines.push(Line::from(vec![
                Span::styled("model   ", Style::default().fg(Color::DarkGray)),
                Span::styled(&m.id, Style::default().add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("family  ", Style::default().fg(Color::DarkGray)),
                Span::raw(
                    app.snapshot
                        .as_ref()
                        .and_then(|s| s.catalog.find_model_anywhere(&m.id, None))
                        .and_then(|(_, e)| e.family.clone())
                        .or_else(|| m.family.clone())
                        .unwrap_or_else(|| "-".into()),
                ),
            ]));
            let caps = [
                ("tools", m.capabilities.tool_call),
                ("reasoning", m.capabilities.reasoning),
                ("cache", m.capabilities.prompt_caching),
            ];
            let spans: Vec<Span> = caps
                .iter()
                .flat_map(|(name, on)| {
                    [
                        Span::styled(
                            if *on { "✔" } else { "✘" },
                            Style::default().fg(if *on { Color::Green } else { Color::Red }),
                        ),
                        Span::raw(format!(" {name}   ")),
                    ]
                })
                .collect();
            lines.push(Line::from(spans));
            if let (Some(ctx), Some(out)) = (m.context_window, m.max_output) {
                lines.push(Line::from(format!("context {ctx} / output {out} tokens")));
            }
            match app.selected_pricing() {
                Some(pc) => {
                    let p = &pc.pricing;
                    lines.push(Line::from(format!(
                        "$/1M in {:.2} · out {:.2} · cache-read {} · cache-write {}",
                        p.input_per_million_usd,
                        p.output_per_million_usd,
                        p.cache_read_per_million_usd
                            .map(|v| format!("{v:.2}"))
                            .unwrap_or_else(|| "—".into()),
                        p.cache_write_per_million_usd
                            .map(|v| format!("{v:.2}"))
                            .unwrap_or_else(|| "—".into()),
                    )));
                    lines.push(Line::from(Span::styled(
                        format!(
                            "pricing: {} snapshot {} ({})",
                            pc.source,
                            pc.snapshot_version.as_deref().unwrap_or("?"),
                            pc.fetched_at.as_deref().unwrap_or("?")
                        ),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                None => {
                    lines.push(Line::from(Span::styled(
                        "pricing: not found in Models.dev → cost will be null",
                        Style::default().fg(Color::Magenta),
                    )));
                }
            }
            for w in app.model_warnings.iter().take(2) {
                lines.push(Line::from(Span::styled(
                    format!("⚠ {w}"),
                    Style::default().fg(Color::Magenta),
                )));
            }
        }
        None => {
            lines.push(Line::from("select a provider to load its models…"));
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::new().borders(Borders::ALL).title(" Model details "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_reasoning(f: &mut Frame, app: &App, area: Rect) {
    let levels = app.visible_reasoning_levels();
    let sel = app.reasoning_idx.min(levels.len().saturating_sub(1));
    let spans: Vec<Span> = levels
        .iter()
        .enumerate()
        .flat_map(|(i, lvl)| {
            let chosen = i == sel;
            [
                Span::styled(
                    format!("[{}]", lvl.as_str()),
                    Style::default()
                        .fg(if chosen { Color::Yellow } else { Color::Gray })
                        .add_modifier(if chosen {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::raw(" "),
            ]
        })
        .collect();
    f.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::new()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " Reasoning ",
                    focused_style(app.focus, Focus::Reasoning),
                ))
                .border_style(focused_style(app.focus, Focus::Reasoning)),
        ),
        area,
    );
}

fn draw_prompts(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .prompts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let default_mark = app
                .config
                .default_prompt
                .as_ref()
                .map(|d| d == &p.name)
                .unwrap_or(false);
            ListItem::new(format!(
                "{}{}{}",
                if i == app.prompt_idx { "▶ " } else { "  " },
                p.name,
                if default_mark { "  ★default" } else { "" }
            ))
        })
        .collect();
    let list = List::new(items).block(
        Block::new()
            .borders(Borders::ALL)
            .title(Span::styled(
                " System prompts (d=set default) ",
                focused_style(app.focus, Focus::Prompts),
            ))
            .border_style(focused_style(app.focus, Focus::Prompts)),
    );
    let mut st = ListState::default().with_selected(Some(
        app.prompt_idx.min(app.prompts.len().saturating_sub(1)),
    ));
    f.render_stateful_widget(list, area, &mut st);
}

fn draw_run(f: &mut Frame, app: &mut App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(66), Constraint::Percentage(34)])
        .split(area);

    let Some(run) = app.run.as_mut() else {
        f.render_widget(
            Paragraph::new("No active run. Configure one in Setup and press Enter on Task."),
            area,
        );
        return;
    };

    // feed (tail by default; ↑/↓ scrolls back via run.scroll)
    let max_lines = cols[0].height.saturating_sub(2) as usize;
    let total = run.feed_lines.len();
    let back = run.scroll.min(total);
    let end = total - back;
    let start = end.saturating_sub(max_lines);
    let visible: Vec<Line> = run.feed_lines[start..end]
        .iter()
        .map(|l| {
            let style = if l.starts_with('✘') || l.starts_with('✖') || l.starts_with('⛔') {
                Style::default().fg(Color::Red)
            } else if l.starts_with('⚠') || l.starts_with('…') {
                Style::default().fg(Color::Magenta)
            } else if l.starts_with('✔') || l.starts_with('■') {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            Line::styled(l.clone(), style)
        })
        .collect();
    let feed = Paragraph::new(visible)
        .block(
            Block::new()
                .borders(Borders::ALL)
                .title(" Live events (events.jsonl tail) "),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(feed, cols[0]);

    // stats panel
    let elapsed = run.started.elapsed().as_secs();
    let u = &run.tokens;
    let hit_ratio = if u.input_tokens > 0 {
        u.cache_read_tokens
            .map(|c| c as f64 / u.input_tokens as f64)
    } else {
        None
    };
    // Live estimate — same math as agent::pricing::compute so the number
    // matches the final statistics.json: cache-write tokens are subtracted
    // from plain input and billed at their own rate when known.
    let cost_est = run.pricing.as_ref().map(|p| {
        let cr = u.cache_read_tokens.unwrap_or(0);
        let cw = u.cache_write_tokens.unwrap_or(0);
        let plain = u.input_tokens.saturating_sub(cr).saturating_sub(cw);
        let cache_write_usd = cw as f64 / 1e6 * p.cache_write_per_million_usd.unwrap_or(0.0);
        plain as f64 / 1e6 * p.input_per_million_usd
            + u.output_tokens as f64 / 1e6 * p.output_per_million_usd
            + cr as f64 / 1e6 * p.cache_read_per_million_usd.unwrap_or(0.0)
            + cache_write_usd
    });
    let mut lines = vec![
        Line::from(format!("provider : {}", run.provider_id)),
        Line::from(format!("model    : {}", run.model_id)),
        Line::from(format!("reasoning: {}", run.reasoning)),
        Line::from(format!("elapsed  : {elapsed}s")),
        Line::from(""),
        Line::from(format!(
            "tokens   : in {} · out {}",
            u.input_tokens, u.output_tokens
        )),
        Line::from(format!(
            "           reason {} · cache-r {} · cache-w {}",
            u.reasoning_tokens
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".into()),
            u.cache_read_tokens
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".into()),
            u.cache_write_tokens
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".into()),
        )),
        Line::from(format!(
            "           total {} · cache-hit {}",
            u.total(),
            hit_ratio
                .map(|r| format!("{r:.4}"))
                .unwrap_or_else(|| "null".into())
        )),
        Line::from(""),
        Line::from(format!("tools    : ✔ {} ✘ {}", run.tool_ok, run.tool_fail)),
        Line::from(format!(
            "errors/warnings: {} / {}",
            run.errors, run.warnings
        )),
        Line::from(format!(
            "cost est.: {}",
            cost_est
                .map(|c| format!("{c:.6} USD"))
                .unwrap_or_else(|| "null (no price)".into())
        )),
    ];
    // Live streaming tail above the counters.
    if !run.live_turn.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("streaming… ({} deltas)", run.delta_count),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        let tail_chars = cols[1].height.saturating_sub(4) as usize * 3;
        let tail: String = {
            let total = run.live_turn.chars().count();
            let skip = total.saturating_sub(tail_chars);
            run.live_turn.chars().skip(skip).collect()
        };
        lines.push(Line::from(Span::raw(tail)));
    }

    let stats = Paragraph::new(lines)
        .block(Block::new().borders(Borders::ALL).title(" Run statistics "))
        .wrap(Wrap { trim: false });
    f.render_widget(stats, cols[1]);

    if let Some(done) = &run.finished_line {
        let bottom = Rect {
            x: cols[0].x,
            y: cols[0].y + cols[0].height.saturating_sub(1),
            width: cols[0].width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                done.clone(),
                Style::default().fg(Color::Green),
            )),
            bottom,
        );
    }
}

fn draw_history(f: &mut Frame, app: &mut App, area: Rect) {
    if app.history.is_empty() {
        f.render_widget(
            Paragraph::new(format!(
                "No previous runs found under {}. Press F5 to rescan.",
                app.output_base.display()
            )),
            area,
        );
        return;
    }

    if let Some(detail) = &app.history_detail {
        f.render_widget(
            Paragraph::new(detail.clone())
                .block(
                    Block::new()
                        .borders(Borders::ALL)
                        .title(" statistics.json (Esc closes) "),
                )
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let rows: Vec<ListItem> = app
        .history
        .iter()
        .map(|h| {
            ListItem::new(format!(
                "{:<10} {:<28} {:<7} {:<12} {:>8}ms  tokens {:<10} ${}",
                h.family,
                truncate(&h.model, 28),
                h.reasoning,
                h.status,
                h.duration_ms.unwrap_or(0),
                h.total_tokens
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "null".into()),
                h.total_usd
                    .map(|c| format!("{c:.6}"))
                    .unwrap_or_else(|| "null".into()),
            ))
        })
        .collect();
    let list = List::new(rows)
        .block(
            Block::new()
                .borders(Borders::ALL)
                .title(" Previous runs (Enter = view statistics.json) "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    let mut st = ListState::default().with_selected(Some(
        app.history_idx.min(app.history.len().saturating_sub(1)),
    ));
    f.render_stateful_widget(list, area, &mut st);
}

fn run_status_line(run: &crate::app::ActiveRun) -> String {
    format!(
        "{} {} [{}] — running {}s{} ",
        run.provider_id,
        run.model_id,
        run.reasoning,
        run.started.elapsed().as_secs(),
        if run.finished_line.is_some() {
            " (done)"
        } else {
            ""
        }
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max - 1).collect();
        format!("{t}…")
    }
}
