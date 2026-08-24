//! Setup screen: providers (searchable/grouped) · models (bulk-selectable)
//! · details/reasoning · system + task prompt pickers.

use crate::state::{Pane, State};
use crate::view::shared::*;
use crate::view::RenderInfo;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

pub fn draw(f: &mut Frame, state: &State, area: ratatui::layout::Rect, info: &mut RenderInfo) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(32),
            Constraint::Percentage(36),
            Constraint::Percentage(32),
        ])
        .split(area);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Ratio(1, 2),
            Constraint::Ratio(1, 2),
        ])
        .split(cols[2]);

    // Pane rects for mouse clicks (indices match Pane::ORDER). right[0]
    // ("Model details") is a read-only info pane — deliberately not
    // focusable, so it maps to no Pane.
    info.setup_panes[0] = cols[0]; // Pane::Providers
    info.setup_panes[1] = cols[1]; // Pane::Models
    info.setup_panes[2] = right[1]; // Pane::Reasoning (reasoning levels)
    info.setup_panes[3] = right[2]; // Pane::Prompts (system prompts)
    info.setup_panes[4] = right[3]; // Pane::Task (task prompts)

    draw_providers(f, state, cols[0], info);
    draw_models(f, state, cols[1], info);

    draw_details(f, state, right[0]);
    draw_reasoning(f, state, right[1]);
    draw_prompts(f, state, right[2]);
    draw_task_prompts(f, state, right[3]);
}

fn draw_providers(
    f: &mut Frame,
    state: &State,
    area: ratatui::layout::Rect,
    info: &mut RenderInfo,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let cursor = if state.setup.focus == Pane::Providers {
        "▏"
    } else {
        ""
    };
    let filter = state.setup.provider_filter.as_str();
    let count = state
        .provider_rows()
        .iter()
        .filter(|r| r.group.is_none())
        .count();
    let search_text = if filter.is_empty() {
        format!(" search: {cursor}")
    } else {
        format!(
            " search: {}{cursor}  — {count} match{}",
            filter,
            if count == 1 { "" } else { "es" }
        )
    };
    f.render_widget(
        Paragraph::new(Span::styled(search_text, Style::default().fg(Color::Cyan))),
        rows[0],
    );

    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_item = 0usize;
    let mut provider_pos = 0usize;
    for row in state.provider_rows() {
        match row.group {
            Some(g) => {
                items.push(ListItem::new(Line::from(Span::styled(
                    format!("── {} ──", g.label()),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ))));
            }
            None => {
                let Some(p) = state.registry.all().get(row.registry_idx) else {
                    continue;
                };
                let mut spans: Vec<Span> = Vec::new();
                if state.prefs.favorites.contains(p.id()) {
                    spans.push(Span::styled("★ ", Style::default().fg(Color::Yellow)));
                }
                spans.extend(highlighted(
                    p.display_name(),
                    filter,
                    Style::default(),
                    Style::default().fg(Color::Yellow),
                ));
                let badge = state.provider_badge(row.registry_idx);
                let badge_color = match badge {
                    "[key ok]" => Color::Green,
                    "[local]" => Color::Blue,
                    _ => Color::Red,
                };
                spans.push(Span::styled(
                    format!(" {badge}"),
                    Style::default().fg(badge_color),
                ));
                let n = state.bulk_count_for(p.id());
                if n > 0 {
                    spans.push(Span::styled(
                        format!(" ☑{n}"),
                        Style::default().fg(Color::Cyan),
                    ));
                }
                items.push(ListItem::new(Line::from(spans)));
                if provider_pos == state.setup.provider_idx {
                    selected_item = items.len() - 1;
                }
                provider_pos += 1;
            }
        }
    }

    let total = items.len();
    let list = List::new(items)
        .block(bordered_block(
            " Providers ",
            focused_style(state.setup.focus, Pane::Providers),
        ))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    let (list_area, bar_area) = scrollbar_area(rows[1], total);
    let mut st = ListState::default().with_selected(Some(selected_item));
    f.render_stateful_widget(list, list_area, &mut st);
    info.offsets.providers = st.offset();
    render_scrollbar(f, bar_area, total, st.offset());
}

fn draw_models(f: &mut Frame, state: &State, area: ratatui::layout::Rect, info: &mut RenderInfo) {
    let checked = state.bulk_checked_indices().len();
    let title = match (state.setup.models_loading, state.setup.model_source) {
        (true, _) => " Models — loading… ".to_string(),
        (false, Some(src)) => format!(" Models [{}] ", src.as_str()),
        (false, None) => " Models [no list available] ".to_string(),
    };
    let title = if checked > 0 {
        format!("{title}☑ {checked}")
    } else {
        title
    };
    let title = if state.setup.multi_select {
        format!("{title} [multi]")
    } else {
        title
    };

    let items: Vec<ListItem> = state
        .setup
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
            let marker = if state.setup.multi_select {
                let checked = state
                    .selected_provider()
                    .map(|p| {
                        state
                            .setup
                            .bulk
                            .contains(&(p.id().to_string(), m.id.clone()))
                    })
                    .unwrap_or(false);
                if checked {
                    "☑ "
                } else {
                    "☐ "
                }
            } else {
                ""
            };
            ListItem::new(format!("{marker}{label}"))
        })
        .collect();
    let total = items.len();
    let list = List::new(items)
        .block(bordered_block(
            title,
            focused_style(state.setup.focus, Pane::Models),
        ))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    let (list_area, bar_area) = scrollbar_area(area, total);
    let mut st = ListState::default().with_selected(Some(
        state
            .setup
            .model_idx
            .min(state.setup.models.len().saturating_sub(1)),
    ));
    f.render_stateful_widget(list, list_area, &mut st);
    info.offsets.models = st.offset();
    render_scrollbar(f, bar_area, total, st.offset());
}

fn draw_details(f: &mut Frame, state: &State, area: ratatui::layout::Rect) {
    let mut lines: Vec<Line> = Vec::new();
    match state.selected_model() {
        Some(m) => {
            lines.push(Line::from(vec![
                Span::styled("model   ", Style::default().fg(Color::DarkGray)),
                Span::styled(&m.id, Style::default().add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("family  ", Style::default().fg(Color::DarkGray)),
                Span::raw(
                    state
                        .setup
                        .snapshot
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
            match state.selected_pricing() {
                Some(pc) => {
                    let p = &pc.pricing;
                    lines.push(Line::from(format!(
                        "$/1M in {:.2} · out {:.2} · cache-r {} · cache-w {}",
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
            for w in state.setup.model_warnings.iter().take(2) {
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
            .block(bordered_block(
                " Model details ",
                Style::default().fg(Color::Gray),
            ))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_reasoning(f: &mut Frame, state: &State, area: ratatui::layout::Rect) {
    let levels = state.visible_reasoning_levels();
    let sel = state
        .setup
        .reasoning_idx
        .min(levels.len().saturating_sub(1));
    let default = state
        .selected_model()
        .and_then(|m| state.prefs.model_defaults.get(&m.id).copied());
    let spans: Vec<Span> = levels
        .iter()
        .enumerate()
        .flat_map(|(i, lvl)| {
            let chosen = i == sel;
            let is_default = default == Some(*lvl);
            [
                Span::styled(
                    format!("[{}{}]", lvl.as_str(), if is_default { "★" } else { "" }),
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
    let title = if default.is_some() {
        " Reasoning (↑/↓ per model, ★=set) "
    } else {
        " Reasoning (↑/↓ per model) "
    };
    f.render_widget(
        Paragraph::new(Line::from(spans)).block(bordered_block(
            title,
            focused_style(state.setup.focus, Pane::Reasoning),
        )),
        area,
    );
}

fn draw_prompts(f: &mut Frame, state: &State, area: ratatui::layout::Rect) {
    let items: Vec<ListItem> = state
        .prompts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let default_mark = state
                .config
                .default_prompt
                .as_ref()
                .map(|d| d == &p.name)
                .unwrap_or(false);
            ListItem::new(format!(
                "{}{}{}",
                if i == state.setup.prompt_idx {
                    "▶ "
                } else {
                    "  "
                },
                p.name,
                if default_mark { "  ★default" } else { "" }
            ))
        })
        .collect();
    let list = List::new(items).block(bordered_block(
        " System prompts (d=set default) ",
        focused_style(state.setup.focus, Pane::Prompts),
    ));
    let mut st = ListState::default().with_selected(Some(
        state
            .setup
            .prompt_idx
            .min(state.prompts.len().saturating_sub(1)),
    ));
    f.render_stateful_widget(list, area, &mut st);
}

fn draw_task_prompts(f: &mut Frame, state: &State, area: ratatui::layout::Rect) {
    let items: Vec<ListItem> = state
        .task_prompts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let default_mark = state
                .config
                .default_task_prompt
                .as_ref()
                .map(|d| d == &p.name)
                .unwrap_or(false);
            ListItem::new(format!(
                "{}{}{}",
                if i == state.setup.task_prompt_idx {
                    "▶ "
                } else {
                    "  "
                },
                p.name,
                if default_mark { "  ★default" } else { "" }
            ))
        })
        .collect();
    let bulk_n = state.setup.bulk.len();
    let title = if bulk_n > 0 {
        format!(" Task prompts (Ctrl-Enter = RUN · x = bulk {bulk_n}) ")
    } else {
        " Task prompts (Ctrl-Enter = RUN · d=set default) ".to_string()
    };
    let list = List::new(items).block(bordered_block(
        title,
        focused_style(state.setup.focus, Pane::Task),
    ));
    let mut st = ListState::default().with_selected(Some(
        state
            .setup
            .task_prompt_idx
            .min(state.task_prompts.len().saturating_sub(1)),
    ));
    f.render_stateful_widget(list, area, &mut st);
}
