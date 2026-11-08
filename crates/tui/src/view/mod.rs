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
use crate::reasoning_map::MapModel;
use crate::state::{Modal, Pane, ProviderRow, State};
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
    /// Run screen session list.
    pub run_list: Rect,
    /// Full-width list regions of the History / Reasoning screens.
    pub history_list: Rect,
    pub map_list: Rect,
    /// Command-palette list area while the palette modal is open (mouse
    /// clicks/rows map to commands).
    pub palette_list: Option<Rect>,
    pub offsets: ListOffsets,
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
        palette::draw(f, state, modal, &mut info);
    }
    info
}

/// Map a mouse click to an action using the geometry of the last drawn frame.
/// Clicks on list rows select them; clicks elsewhere focus setup panes.
///
/// While a modal is open the screen behind it must not react to clicks.
pub fn mouse_action(state: &State, info: &RenderInfo, col: u16, row: u16) -> Option<Action> {
    if state.modal.is_some() && !matches!(state.modal, Some(Modal::Palette { .. })) {
        return None;
    }
    let pos = Position::new(col, row);
    if let Some(Modal::Palette { .. }) = &state.modal {
        // Command rows are clickable (palette mouse parity). The recorded
        // rect is the list area itself (no border), so row 0 is `rect.y`.
        let rect = info.palette_list?;
        if rect.contains(pos) {
            return Some(Action::PaletteRunAction((row - rect.y) as usize));
        }
        return None;
    }
    let pos = Position::new(col, row);
    if info.tab_bar.contains(pos) {
        let idx = tab_at(info.tab_bar, col)?;
        return Some(Action::SwitchScreen(Screen::ALL[idx]));
    }
    match state.screen {
        Screen::Setup => {
            for (i, pane) in Pane::ORDER.iter().enumerate() {
                let rect = info.setup_panes[i];
                if rect.contains(pos) {
                    return match pane {
                        Pane::Providers => provider_row_at(
                            &state.provider_rows(),
                            info.offsets.providers,
                            rect,
                            row,
                        )
                        .map(|idx| Action::MouseSelectRow {
                            target: SelectTarget::Providers,
                            idx,
                        }),
                        Pane::Models => {
                            flat_row_at(info.offsets.models, rect, row, state.setup.models.len())
                                .map(|idx| Action::MouseSelectRow {
                                    target: SelectTarget::Models,
                                    idx,
                                })
                        }
                        _ => Some(Action::FocusPane(*pane)),
                    };
                }
            }
            None
        }
        Screen::Run => {
            if info.run_list.contains(pos) {
                flat_row_at(
                    info.offsets.sessions,
                    info.run_list,
                    row,
                    state.runs.runs.len(),
                )
                .map(|idx| Action::MouseSelectRow {
                    target: SelectTarget::Sessions,
                    idx,
                })
            } else {
                None
            }
        }
        Screen::History => {
            if info.history_list.contains(pos) {
                flat_row_at(
                    info.offsets.history,
                    info.history_list,
                    row,
                    state.history.rows.len(),
                )
                .map(|idx| Action::MouseSelectRow {
                    target: SelectTarget::History,
                    idx,
                })
            } else {
                None
            }
        }
        Screen::Reasoning => {
            if info.map_list.contains(pos) {
                map_row_at(&state.map_rows(), info.offsets.map, info.map_list, row).map(|idx| {
                    Action::MouseSelectRow {
                        target: SelectTarget::Map,
                        idx,
                    }
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
            // Wheel up = previous command (↑).
            Modal::Palette { .. } => Some(Action::PaletteMove(-dir)),
            _ => None,
        };
    }
    match state.screen {
        Screen::Setup => {
            for (i, pane) in Pane::ORDER.iter().enumerate() {
                if info.setup_panes[i].contains(pos) {
                    return match pane {
                        // The reasoning-levels strip cycles instead of
                        // scrolling (wheel up = previous, like ↑).
                        Pane::Reasoning => Some(Action::CycleReasoning(-dir)),
                        // List panes move their selection: wheel up (dir=1)
                        // must behave like ↑ (previous item), so negate.
                        _ => Some(Action::MouseWheelSetup {
                            pane: *pane,
                            dir: -dir,
                        }),
                    };
                }
            }
            None
        }
        // Transcript/detail scrolls use the "offset from bottom" convention:
        // wheel up = move back through history, same as ↑.
        Screen::Run => Some(Action::ScrollTranscript(dir)),
        Screen::History => Some(Action::MoveHistory(-dir)),
        Screen::Reasoning => Some(Action::MapMove(-dir)),
    }
}

/// Item index at `row` for a flat list (no group headers). `row` is inside
/// the list's bordered area; `offset` is the list's first visible item.
/// Returns `None` for empty space (below the last item) — a click there must
/// not change the selection.
fn flat_row_at(offset: usize, rect: Rect, row: u16, len: usize) -> Option<usize> {
    let raw = offset as i64 + (row as i64 - rect.y as i64 - 1);
    if raw < 0 || raw >= len as i64 {
        return None;
    }
    Some(raw as usize)
}

/// Non-header provider row index at `row` (provider items are interleaved
/// with group headers). `None` = empty space or a group header: no change.
fn provider_row_at(rows: &[ProviderRow], offset: usize, rect: Rect, row: u16) -> Option<usize> {
    let target = offset as i64 + (row as i64 - rect.y as i64 - 1);
    if target < 0 || target as usize >= rows.len() {
        return None;
    }
    let mut non_header = 0usize;
    for (i, r) in rows.iter().enumerate() {
        if i as i64 == target {
            // Process the target row before stopping: the previous code broke
            // first, selecting the row *above* the click.
            return r.group.is_none().then_some(non_header);
        }
        if r.group.is_none() {
            non_header += 1;
        }
    }
    None
}

/// Reasoning-map model index at `row` (provider header lines interleaved).
/// A click on a group header selects that group's first model; a click below
/// the last line selects nothing.
fn map_row_at(models: &[MapModel], offset: usize, rect: Rect, row: u16) -> Option<usize> {
    let target = offset as i64 + (row as i64 - rect.y as i64 - 1);
    if target < 0 {
        return None;
    }
    // Total rendered lines (models + one header per provider group).
    let mut total = 0i64;
    let mut last_provider: Option<&str> = None;
    for m in models {
        if last_provider != Some(m.provider_id.as_str()) {
            last_provider = Some(m.provider_id.as_str());
            total += 1;
        }
        total += 1;
    }
    if target >= total {
        return None;
    }

    let mut line = 0i64;
    let mut last_provider: Option<&str> = None;
    for (i, m) in models.iter().enumerate() {
        if last_provider != Some(m.provider_id.as_str()) {
            last_provider = Some(m.provider_id.as_str());
            // Clicking a provider header selects its first model.
            if line == target {
                return Some(i);
            }
            line += 1;
        }
        if line == target {
            return Some(i);
        }
        line += 1;
    }
    None
}

/// Which tab a click at `col` hits. Tab boxes are the cumulative widths of
/// the rendered titles (each followed by one divider column); clicks in the
/// trailing empty space hit no tab.
fn tab_at(tab_bar: Rect, col: u16) -> Option<usize> {
    let mut x = tab_bar.x;
    for (i, screen) in Screen::ALL.iter().enumerate() {
        let width = screen.title().chars().count() as u16;
        if col >= x && col < x.saturating_add(width) {
            return Some(i);
        }
        x = x.saturating_add(width + 1); // +1 divider column
    }
    None
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
        let (mut state, _dir) = crate::testutil::test_state();
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

        // Model pane (flat list): row 3 of the pane is item 1 (row 2 is the
        // first item after the border).
        state.setup.models = vec![
            lmhub_core::ModelInfo::bare("m1"),
            lmhub_core::ModelInfo::bare("m2"),
        ];
        assert!(matches!(
            mouse_action(&state, &info, 50, 3),
            Some(Action::MouseSelectRow {
                target: SelectTarget::Models,
                idx: 1
            })
        ));
        // Empty space below the last item changes nothing.
        assert!(mouse_action(&state, &info, 50, 10).is_none());
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
        // Titles: "[1] Setup"(9) "[2] Run"(7) "[3] History"(11) "[4] Reasoning"(13)
        // separated by one divider column each.
        assert!(matches!(
            mouse_action(&state, &info, 12, 0),
            Some(Action::SwitchScreen(Screen::Run))
        ));
        assert!(matches!(
            mouse_action(&state, &info, 32, 0),
            Some(Action::SwitchScreen(Screen::Reasoning))
        ));
    }

    #[test]
    fn palette_rows_are_clickable() {
        let (mut state, _dir) = crate::testutil::test_state();
        state.modal = Some(Modal::Palette {
            filter: crate::input::EditField::new(),
            cursor: 0,
        });
        let info = RenderInfo {
            palette_list: Some(rect(10, 5, 40, 8)),
            ..Default::default()
        };
        assert!(matches!(
            mouse_action(&state, &info, 12, 5),
            Some(Action::PaletteRunAction(0))
        ));
        assert!(matches!(
            mouse_action(&state, &info, 12, 7),
            Some(Action::PaletteRunAction(2))
        ));
        // Clicks on the filter line / outside the list do nothing.
        assert!(mouse_action(&state, &info, 12, 3).is_none());
        assert!(mouse_action(&state, &info, 12, 20).is_none());
    }

    #[test]
    fn clicks_are_ignored_while_a_modal_is_open() {
        let (mut state, _dir) = crate::testutil::test_state();
        state.modal = Some(Modal::Help);
        let info = RenderInfo {
            tab_bar: rect(0, 0, 100, 1),
            setup_panes: {
                let mut p = [Rect::default(); 5];
                p[0] = rect(0, 1, 40, 30);
                p
            },
            ..Default::default()
        };
        assert!(mouse_action(&state, &info, 12, 0).is_none(), "tabs");
        assert!(mouse_action(&state, &info, 10, 10).is_none(), "panes");
    }

    #[test]
    fn flat_row_at_maps_border_last_row_and_empty_space() {
        let r = rect(0, 1, 10, 10);
        assert_eq!(flat_row_at(0, r, 2, 3), Some(0), "first visible row");
        assert_eq!(flat_row_at(0, r, 4, 3), Some(2), "last item");
        assert_eq!(flat_row_at(0, r, 5, 3), None, "empty space below");
        assert_eq!(flat_row_at(0, r, 1, 3), None, "border/title line");
        assert_eq!(flat_row_at(2, r, 2, 3), Some(2), "scrolled offset");
        assert_eq!(flat_row_at(10, r, 2, 3), None, "offset out of range");
        assert_eq!(flat_row_at(0, r, 2, 0), None, "empty list");
    }

    #[test]
    fn provider_row_at_handles_headers_and_bounds() {
        use crate::provider_search::Group;
        let rows = vec![
            ProviderRow {
                group: Some(Group::Native),
                registry_idx: 0,
            },
            ProviderRow {
                group: None,
                registry_idx: 1,
            },
            ProviderRow {
                group: None,
                registry_idx: 2,
            },
            ProviderRow {
                group: Some(Group::Routed),
                registry_idx: 3,
            },
            ProviderRow {
                group: None,
                registry_idx: 4,
            },
        ];
        let r = rect(0, 1, 10, 10);
        assert_eq!(provider_row_at(&rows, 0, r, 1), None, "border");
        assert_eq!(provider_row_at(&rows, 0, r, 2), None, "header row");
        assert_eq!(provider_row_at(&rows, 0, r, 3), Some(0), "first provider");
        assert_eq!(provider_row_at(&rows, 0, r, 4), Some(1));
        assert_eq!(provider_row_at(&rows, 0, r, 5), None, "second header");
        assert_eq!(
            provider_row_at(&rows, 0, r, 6),
            Some(2),
            "row after a header is not off by one"
        );
        assert_eq!(provider_row_at(&rows, 0, r, 7), None, "empty space");
    }

    #[test]
    fn map_row_at_headers_and_empty_space() {
        use lmhub_core::ReasoningLevel;
        let models = vec![
            MapModel {
                provider_id: "a".into(),
                model_id: "m1".into(),
                levels: Some(vec![ReasoningLevel::Off]),
                reasoning: true,
            },
            MapModel {
                provider_id: "a".into(),
                model_id: "m2".into(),
                levels: None,
                reasoning: true,
            },
            MapModel {
                provider_id: "b".into(),
                model_id: "m3".into(),
                levels: None,
                reasoning: false,
            },
        ];
        let r = rect(0, 1, 10, 10);
        // Rendered lines: header(a), m1, m2, header(b), m3.
        assert_eq!(
            map_row_at(&models, 0, r, 2),
            Some(0),
            "header → first model"
        );
        assert_eq!(map_row_at(&models, 0, r, 3), Some(0), "m1");
        assert_eq!(map_row_at(&models, 0, r, 4), Some(1), "m2");
        assert_eq!(map_row_at(&models, 0, r, 5), Some(2), "header → m3");
        assert_eq!(map_row_at(&models, 0, r, 6), Some(2), "m3");
        assert_eq!(map_row_at(&models, 0, r, 7), None, "empty space");
        assert_eq!(map_row_at(&models, 0, r, 1), None, "border");
    }

    #[test]
    fn tab_at_uses_rendered_title_widths() {
        let bar = rect(0, 0, 100, 1);
        assert_eq!(tab_at(bar, 0), Some(0));
        assert_eq!(tab_at(bar, 8), Some(0), "last column of [1] Setup");
        assert_eq!(tab_at(bar, 9), None, "divider");
        assert_eq!(tab_at(bar, 10), Some(1));
        assert_eq!(tab_at(bar, 18), Some(2));
        assert_eq!(tab_at(bar, 30), Some(3));
        assert_eq!(tab_at(bar, 43), None, "divider after the last tab");
        assert_eq!(tab_at(bar, 44), None, "trailing empty space");
        assert_eq!(tab_at(rect(5, 0, 100, 1), 5), Some(0), "bar x offset");
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
                dir: -1
            })
        ));
        assert!(matches!(
            mouse_wheel_action(&state, &info, 85, 8, -1),
            Some(Action::CycleReasoning(1))
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
            Some(Action::MoveHistory(1))
        ));
        state.screen = Screen::Reasoning;
        assert!(matches!(
            mouse_wheel_action(&state, &info, 50, 10, 1),
            Some(Action::MapMove(-1))
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
