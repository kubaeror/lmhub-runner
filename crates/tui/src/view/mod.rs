//! Draw dispatch: chrome (tabs + footer), per-screen views, modal overlay.
//!
//! Drawing is **pure** — it never mutates [`State`]. The [`RenderInfo`] it
//! returns captures this frame's geometry and list scroll offsets, which
//! input handling (mouse clicks and wheel) then consumes. This removes the
//! old mutation-during-draw layout cache that could map clicks with stale
//! rectangles.

pub mod history;
pub mod palette;
pub mod reasoning;
pub mod run;
pub mod setup;
pub mod shared;

use crate::action::{Action, Screen, SelectTarget};
use crate::state::{Modal, Pane, State};
use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Tabs},
    Frame,
};

/// First visible item index of each stateful list, read back from the
/// `ListState` after rendering (so mouse row hits map exactly even when the
/// list is scrolled).
#[derive(Debug, Clone, Copy, Default)]
pub struct ListOffsets {
    pub providers: usize,
    pub models: usize,
    pub sessions: usize,
    pub history: usize,
    pub map: usize,
}

/// Geometry of the most recently drawn frame, returned by [`draw`] and
/// consumed by [`mouse_action`] / [`mouse_wheel_action`]. All-zero by
/// default, so events before the first draw map to nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderInfo {
    pub tab_bar: Rect,
    /// Setup panes, indexed by `Pane::ORDER`.
    pub setup_panes: [Rect; 5],
    /// Run screen: `[0]` session list, `[1]` transcript.
    pub run_panes: [Rect; 2],
    /// Full-width list regions of the History / Reasoning screens.
    pub history_list: Rect,
    pub map_list: Rect,
    pub offsets: ListOffsets,
    /// First visible transcript line of the selected run.
    pub transcript: usize,
    /// Total transcript lines (scrollbar extent).
    pub transcript_total: usize,
}

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 12;

pub fn draw(f: &mut Frame, state: &State) -> RenderInfo {
    if f.area().width < MIN_WIDTH || f.area().height < MIN_HEIGHT {
        f.render_widget(
            Paragraph::new(format!(
                "terminal too small — enlarge to at least {MIN_WIDTH}x{MIN_HEIGHT}"
            )),
            f.area(),
        );
        return RenderInfo::default();
    }

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

    let mut info = RenderInfo {
        tab_bar: chunks[0],
        ..Default::default()
    };
    match state.screen {
        Screen::Setup => setup::draw(f, state, chunks[1], &mut info),
        Screen::Run => run::draw(f, state, chunks[1], &mut info),
        Screen::History => history::draw(f, state, chunks[1], &mut info),
        Screen::Reasoning => reasoning::draw(f, state, chunks[1], &mut info),
    }

    let status = shared::status_line(state);
    let hint = shared::hints(state);
    let max_hint = chunks[2]
        .width
        .saturating_sub(status.chars().count() as u16 + 2);
    let hint = shared::truncate(&hint, max_hint as usize);
    let footer = Line::from(vec![
        Span::styled(status, Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(hint, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[2]);

    if let Some(modal) = &state.modal {
        palette::draw(f, state, modal);
    }
    info
}

/// Map a mouse click to an action using the geometry of the last drawn frame.
/// Clicks on list rows select them; clicks elsewhere focus setup panes.
pub fn mouse_action(state: &State, info: &RenderInfo, col: u16, row: u16) -> Option<Action> {
    let pos = Position::new(col, row);
    if info.tab_bar.contains(pos) {
        let tab_bar = info.tab_bar;
        let n = Screen::ALL.len() as u16;
        let per = tab_bar.width.div_ceil(n).max(1);
        let idx = (col.saturating_sub(tab_bar.x)) / per;
        let idx = (idx as usize).min(Screen::ALL.len() - 1);
        return Some(Action::SwitchScreen(Screen::ALL[idx]));
    }
    match state.screen {
        Screen::Setup => {
            for (i, pane) in Pane::ORDER.iter().enumerate() {
                let rect = info.setup_panes[i];
                if rect.contains(pos) {
                    return match pane {
                        Pane::Providers => Some(Action::MouseSelectRow {
                            target: SelectTarget::Providers,
                            idx: provider_row_at(state, info.offsets.providers, rect, row),
                        }),
                        Pane::Models => Some(Action::MouseSelectRow {
                            target: SelectTarget::Models,
                            idx: flat_row_at(
                                info.offsets.models,
                                rect,
                                row,
                                state.setup.models.len(),
                            ),
                        }),
                        _ => Some(Action::FocusPane(*pane)),
                    };
                }
            }
            None
        }
        Screen::Run => {
            if info.run_panes[0].contains(pos) {
                Some(Action::MouseSelectRow {
                    target: SelectTarget::Sessions,
                    idx: flat_row_at(
                        info.offsets.sessions,
                        info.run_panes[0],
                        row,
                        state.runs.runs.len(),
                    ),
                })
            } else {
                None
            }
        }
        Screen::History => {
            if info.history_list.contains(pos) {
                Some(Action::MouseSelectRow {
                    target: SelectTarget::History,
                    idx: flat_row_at(
                        info.offsets.history,
                        info.history_list,
                        row,
                        state.history.rows.len(),
                    ),
                })
            } else {
                None
            }
        }
        Screen::Reasoning => {
            if info.map_list.contains(pos) {
                Some(Action::MouseSelectRow {
                    target: SelectTarget::Map,
                    idx: map_row_at(state, info.offsets.map, info.map_list, row),
                })
            } else {
                None
            }
        }
    }
}

/// Map a mouse wheel tick to an action: scroll the focused content, focus +
/// move setup panes, or scroll the open modal.
pub fn mouse_wheel_action(
    state: &State,
    info: &RenderInfo,
    col: u16,
    row: u16,
    dir: i32,
) -> Option<Action> {
    let pos = Position::new(col, row);
    // Modals own wheel scrolling.
    if let Some(modal) = &state.modal {
        return match modal {
            Modal::HistoryDetail { .. } => Some(Action::ScrollHistoryDetail(dir)),
            Modal::RunDetail { .. } => Some(Action::ScrollTranscript(dir)),
            _ => None,
        };
    }
    match state.screen {
        Screen::Setup => {
            for (i, pane) in Pane::ORDER.iter().enumerate() {
                if info.setup_panes[i].contains(pos) {
                    return match pane {
                        // The reasoning-levels strip cycles instead of
                        // scrolling.
                        Pane::Reasoning => Some(Action::CycleReasoning(dir)),
                        _ => Some(Action::MouseWheelSetup { pane: *pane, dir }),
                    };
                }
            }
            None
        }
        Screen::Run => Some(Action::ScrollTranscript(dir)),
        Screen::History => Some(Action::MoveHistory(dir)),
        Screen::Reasoning => Some(Action::MapMove(dir)),
    }
}

/// Item index at `row` for a flat list (no group headers). `row` is inside
/// the list's bordered area; `offset` is the list's first visible item.
fn flat_row_at(offset: usize, rect: Rect, row: u16, len: usize) -> usize {
    let raw = offset as i64 + (row as i64 - rect.y as i64 - 1);
    raw.clamp(0, len.saturating_sub(1) as i64) as usize
}

/// Non-header provider row index at `row` (provider items are interleaved
/// with group headers).
fn provider_row_at(state: &State, offset: usize, rect: Rect, row: u16) -> usize {
    let target = offset as i64 + (row as i64 - rect.y as i64 - 1);
    let rows = state.provider_rows();
    let mut non_header = 0usize;
    let mut selected = 0usize;
    for (i, r) in rows.iter().enumerate() {
        if i as i64 >= target {
            break;
        }
        if r.group.is_none() {
            selected = non_header;
            non_header += 1;
        }
    }
    selected.min(
        rows.iter()
            .filter(|r| r.group.is_none())
            .count()
            .saturating_sub(1),
    )
}

/// Reasoning-map model index at `row` (provider header lines interleaved).
fn map_row_at(state: &State, offset: usize, rect: Rect, row: u16) -> usize {
    let target = offset as i64 + (row as i64 - rect.y as i64 - 1);
    let models = state.map_rows();
    let mut line = 0i64;
    let mut last_provider = String::new();
    let mut selected = 0usize;
    for (i, m) in models.iter().enumerate() {
        if m.provider_id != last_provider {
            last_provider = m.provider_id.clone();
            // Clicking a provider header selects its first model.
            if line == target {
                return i;
            }
            line += 1;
        }
        if line == target {
            selected = i;
        }
        line += 1;
    }
    selected.min(models.len().saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Pane;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn setup_mouse_maps_clickable_panes() {
        let (state, _dir) = crate::testutil::test_state();
        let info = RenderInfo {
            tab_bar: rect(0, 0, 100, 1),
            setup_panes: {
                let mut p = [Rect::default(); 5];
                p[0] = rect(0, 1, 40, 30);
                p[1] = rect(40, 1, 40, 30);
                p[2] = rect(80, 7, 20, 3);
                p[3] = rect(80, 10, 20, 7);
                p[4] = rect(80, 17, 20, 7);
                p
            },
            ..Default::default()
        };

        // Model pane (flat list): click maps to a row index (clamped to the
        // empty models list → 0).
        assert!(matches!(
            mouse_action(&state, &info, 50, 10),
            Some(Action::MouseSelectRow {
                target: SelectTarget::Models,
                idx: 0
            })
        ));
        // Reasoning / prompts / task focus without selecting.
        assert!(matches!(
            mouse_action(&state, &info, 85, 8),
            Some(Action::FocusPane(Pane::Reasoning))
        ));
        assert!(matches!(
            mouse_action(&state, &info, 85, 13),
            Some(Action::FocusPane(Pane::Prompts))
        ));
        assert!(matches!(
            mouse_action(&state, &info, 85, 20),
            Some(Action::FocusPane(Pane::Task))
        ));
        // Clicking the read-only "Model details" region focuses nothing.
        assert!(mouse_action(&state, &info, 85, 3).is_none());
    }

    #[test]
    fn mouse_clicks_tab_bar_switches_screen() {
        let (state, _dir) = crate::testutil::test_state();
        let info = RenderInfo {
            tab_bar: rect(0, 0, 100, 1),
            ..Default::default()
        };
        // 100/4 = 25 per tab: [1]Setup 0-24, [2]Run 25-49, [3]History 50-74,
        // [4]Reasoning 75-99.
        assert!(matches!(
            mouse_action(&state, &info, 30, 0),
            Some(Action::SwitchScreen(Screen::Run))
        ));
        assert!(matches!(
            mouse_action(&state, &info, 85, 0),
            Some(Action::SwitchScreen(Screen::Reasoning))
        ));
    }

    #[test]
    fn wheel_scrolls_setup_panes_and_screens() {
        let (state, _dir) = crate::testutil::test_state();
        let info = RenderInfo {
            tab_bar: rect(0, 0, 100, 1),
            setup_panes: {
                let mut p = [Rect::default(); 5];
                p[0] = rect(0, 1, 40, 30);
                p[1] = rect(40, 1, 40, 30);
                p[2] = rect(80, 7, 20, 3);
                p
            },
            ..Default::default()
        };
        assert!(matches!(
            mouse_wheel_action(&state, &info, 10, 10, 1),
            Some(Action::MouseWheelSetup {
                pane: Pane::Providers,
                dir: 1
            })
        ));
        assert!(matches!(
            mouse_wheel_action(&state, &info, 85, 8, -1),
            Some(Action::CycleReasoning(-1))
        ));
        // Wheel over "Model details" (unmapped rect) does nothing.
        assert!(mouse_wheel_action(&state, &info, 85, 3, 1).is_none());
    }

    #[test]
    fn wheel_on_run_history_map_uses_existing_actions() {
        let (mut state, _dir) = crate::testutil::test_state();
        state.screen = Screen::Run;
        let info = RenderInfo::default();
        assert!(matches!(
            mouse_wheel_action(&state, &info, 50, 10, 1),
            Some(Action::ScrollTranscript(1))
        ));
        state.screen = Screen::History;
        assert!(matches!(
            mouse_wheel_action(&state, &info, 50, 10, -1),
            Some(Action::MoveHistory(-1))
        ));
        state.screen = Screen::Reasoning;
        assert!(matches!(
            mouse_wheel_action(&state, &info, 50, 10, 1),
            Some(Action::MapMove(1))
        ));
    }

    #[test]
    fn wheel_inside_modal_scrolls_the_modal() {
        let (mut state, _dir) = crate::testutil::test_state();
        state.modal = Some(Modal::HistoryDetail {
            text: "line".into(),
            scroll: 0,
        });
        assert!(matches!(
            mouse_wheel_action(&state, &RenderInfo::default(), 50, 10, 1),
            Some(Action::ScrollHistoryDetail(1))
        ));
        state.modal = Some(Modal::RunDetail { run_id: 1 });
        assert!(matches!(
            mouse_wheel_action(&state, &RenderInfo::default(), 50, 10, -1),
            Some(Action::ScrollTranscript(-1))
        ));
    }
}
