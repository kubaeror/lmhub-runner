//! Key dispatch: raw crossterm events → [`Action`]s, aware of the current
//! modal and screen. A single table per context — adding a binding touches
//! exactly one place.

use crate::action::Action;
use crate::state::{Modal, Pane, State};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Global bindings that work on every screen.
pub fn dispatch(state: &State, key: KeyEvent) -> Option<Action> {
    if key.kind != KeyEventKind::Press {
        return None;
    }

    // Ctrl-C / Ctrl-Q always quit (graceful; second press force-quits).
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q'))
    {
        return Some(quit_action(state));
    }

    if let Some(modal) = &state.modal {
        return modal_keys(modal, key);
    }

    match key.code {
        // Bare `q` quits — except in a type-to-filter field (provider
        // search / reasoning map), where `q` is a filter character.
        KeyCode::Char('q') if !is_filter_context(state) => return Some(quit_action(state)),
        KeyCode::Char(':') => return Some(Action::OpenPalette),
        KeyCode::Char('?') => return Some(Action::OpenHelp),
        KeyCode::Tab => {
            return Some(Action::SwitchScreen(state.screen.cycle(true)));
        }
        _ => {}
    }

    match state.screen {
        crate::action::Screen::Setup => setup_keys(state, key),
        crate::action::Screen::Run => run_keys(key),
        crate::action::Screen::History => history_keys(key),
        crate::action::Screen::Reasoning => reasoning_map_keys(state, key),
    }
}

/// Graceful quit, or force-quit when a cancel is already in flight.
fn quit_action(state: &State) -> Action {
    let still_running = state
        .runs
        .runs
        .iter()
        .any(|r| r.status == crate::state::RunSessionStatus::Running);
    if still_running && state.cancel_requested {
        Action::ForceQuit
    } else {
        Action::Quit
    }
}

/// True when the focused context is a type-to-filter field, where printable
/// characters (including `q`) belong to the filter rather than to the app.
fn is_filter_context(state: &State) -> bool {
    match state.screen {
        crate::action::Screen::Setup => state.setup.focus == Pane::Providers,
        crate::action::Screen::Reasoning => true,
        _ => false,
    }
}

/// Keys while a modal is open: the modal owns the input stream.
fn modal_keys(modal: &Modal, key: KeyEvent) -> Option<Action> {
    match modal {
        Modal::EnterKey { .. } => match key.code {
            KeyCode::Esc => Some(Action::CloseModal),
            KeyCode::Enter => Some(Action::SaveKey),
            KeyCode::Backspace => Some(Action::EnterKeyBackspace),
            KeyCode::Delete => Some(Action::EnterKeyDelete),
            KeyCode::Left => Some(Action::EnterKeyCursor(-1)),
            KeyCode::Right => Some(Action::EnterKeyCursor(1)),
            KeyCode::Char(c) => Some(Action::EnterKeyChar(c)),
            _ => None,
        },
        Modal::Palette { .. } => match key.code {
            KeyCode::Esc => Some(Action::CloseModal),
            KeyCode::Backspace => Some(Action::PaletteBackspace),
            KeyCode::Up => Some(Action::PaletteMove(-1)),
            KeyCode::Down => Some(Action::PaletteMove(1)),
            KeyCode::Enter => Some(Action::PaletteEnter),
            KeyCode::Char(c) => Some(Action::PaletteChar(c)),
            _ => None,
        },
        Modal::BulkConfirm => match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Some(Action::CloseModal),
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                Some(Action::ConfirmBulkStart)
            }
            _ => None,
        },
        Modal::Help => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => {
                Some(Action::CloseModal)
            }
            _ => None,
        },
        Modal::HistoryDetail { .. } => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => Some(Action::CloseModal),
            KeyCode::Up => Some(Action::ScrollHistoryDetail(1)),
            KeyCode::Down => Some(Action::ScrollHistoryDetail(-1)),
            _ => None,
        },
        Modal::RunDetail { .. } => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => Some(Action::CloseModal),
            KeyCode::Up => Some(Action::ScrollTranscript(1)),
            KeyCode::Down => Some(Action::ScrollTranscript(-1)),
            _ => None,
        },
    }
}

fn setup_keys(state: &State, key: KeyEvent) -> Option<Action> {
    let focus = state.setup.focus;
    match key.code {
        // Pane navigation works from anywhere.
        KeyCode::Left => return Some(Action::CycleFocus(false)),
        KeyCode::Right => return Some(Action::CycleFocus(true)),
        KeyCode::F(5) => return Some(Action::RefreshModels(true)),
        KeyCode::Char('r') if !matches!(focus, Pane::Providers | Pane::Task) => {
            return Some(Action::RefreshModels(false));
        }
        _ => {}
    }
    match focus {
        Pane::Providers => match key.code {
            KeyCode::Up => Some(Action::MoveSelection(-1)),
            KeyCode::Down => Some(Action::MoveSelection(1)),
            KeyCode::Enter => Some(Action::ConnectProvider),
            KeyCode::Esc => Some(Action::ClearSearch),
            KeyCode::Backspace => provider_search_backspace(state),
            KeyCode::Char('F') => Some(Action::ToggleFavorite),
            KeyCode::Char(c) => provider_search_char(state, c),
            _ => None,
        },
        Pane::Models => match key.code {
            KeyCode::Up => Some(Action::MoveSelection(-1)),
            KeyCode::Down => Some(Action::MoveSelection(1)),
            KeyCode::Enter => Some(Action::SelectModel),
            KeyCode::Char(' ') => Some(Action::ToggleBulk),
            KeyCode::Char('m') => Some(Action::ToggleMultiSelect),
            KeyCode::Char('C') => Some(Action::ClearBulk),
            _ => None,
        },
        Pane::Reasoning => match key.code {
            KeyCode::Up => Some(Action::CycleReasoning(-1)),
            KeyCode::Down => Some(Action::CycleReasoning(1)),
            KeyCode::Char('d') => Some(Action::SetModelDefault),
            _ => None,
        },
        Pane::Prompts => match key.code {
            KeyCode::Up => Some(Action::CyclePrompt(-1)),
            KeyCode::Down => Some(Action::CyclePrompt(1)),
            KeyCode::Enter | KeyCode::Char('d') => Some(Action::SetDefaultPrompt),
            _ => None,
        },
        Pane::Task => match key.code {
            // Up/Down must match every other pane: Up moves back, Down forward.
            KeyCode::Up => Some(Action::CycleTaskPrompt(-1)),
            KeyCode::Down => Some(Action::CycleTaskPrompt(1)),
            KeyCode::Char('d') => Some(Action::SetDefaultTaskPrompt),
            // Ctrl+Enter runs the selected task prompt — chat-UI convention.
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::StartRun)
            }
            KeyCode::Char('x') => Some(Action::BulkStart),
            _ => None,
        },
    }
}

/// Auto-filter: typing in the providers pane edits the search text — but
/// reserved keys keep their action.
fn provider_search_char(state: &State, c: char) -> Option<Action> {
    let mut text = state.setup.provider_filter.as_str().to_string();
    text.push(c);
    Some(Action::SearchProviders(text))
}

/// Backspace in the providers search removes the last filter character.
fn provider_search_backspace(state: &State) -> Option<Action> {
    let mut text = state.setup.provider_filter.as_str().to_string();
    text.pop();
    Some(Action::SearchProviders(text))
}

fn run_keys(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Up => Some(Action::ScrollTranscript(1)),
        KeyCode::Down => Some(Action::ScrollTranscript(-1)),
        KeyCode::Char('[') => Some(Action::PrevSession),
        KeyCode::Char(']') => Some(Action::NextSession),
        KeyCode::Char('c') => Some(Action::CancelSession),
        KeyCode::Char('C') => Some(Action::CancelAllRuns),
        KeyCode::Char('R') => Some(Action::RerunSession),
        KeyCode::Char('v') => Some(Action::ToggleRawFeed),
        KeyCode::Enter => Some(Action::OpenRunDetail),
        _ => None,
    }
}

fn history_keys(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Up => Some(Action::MoveHistory(-1)),
        KeyCode::Down => Some(Action::MoveHistory(1)),
        KeyCode::F(5) => Some(Action::RescanHistory),
        KeyCode::Enter => Some(Action::OpenHistoryDetail),
        _ => None,
    }
}

fn reasoning_map_keys(state: &State, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Up => Some(Action::MapMove(-1)),
        KeyCode::Down => Some(Action::MapMove(1)),
        KeyCode::Esc => Some(Action::MapClear),
        KeyCode::F(5) => Some(Action::ReloadSnapshot),
        // Uppercase: lowercase letters belong to the live filter.
        KeyCode::Char('D') => Some(Action::CycleModelDefault),
        KeyCode::Char(c) => Some(Action::MapFilter(format!(
            "{}{}",
            state.map.filter.as_str(),
            c
        ))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A modal-less state for dispatch tests.
    fn state_with() -> State {
        crate::testutil::test_state().0
    }

    #[test]
    fn global_bindings() {
        let mut s = state_with();
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char(':'))),
            Some(Action::OpenPalette)
        ));
        assert!(matches!(
            dispatch(&s, key(KeyCode::Tab)),
            Some(Action::SwitchScreen(crate::action::Screen::Run))
        ));
        // Ctrl-C quits even from a type-to-filter context (providers pane).
        assert!(matches!(
            dispatch(&s, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        ));
        // `q` quits from a non-filter screen…
        s.screen = crate::action::Screen::Run;
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('q'))),
            Some(Action::Quit)
        ));
        // …but on the Setup providers pane it is a search character.
        s.screen = crate::action::Screen::Setup;
        s.setup.focus = Pane::Providers;
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('q'))),
            Some(Action::SearchProviders(text)) if text == "q"
        ));
        s.modal = Some(Modal::EnterKey {
            provider_id: "x".into(),
            input: "".into(),
        });
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('q'))),
            Some(Action::EnterKeyChar('q'))
        ));
        assert!(matches!(
            dispatch(&s, key(KeyCode::Esc)),
            Some(Action::CloseModal)
        ));
    }

    #[test]
    fn setup_auto_filter_typing() {
        let s = state_with();
        // Providers pane is the default focus: typing searches.
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('g'))),
            Some(Action::SearchProviders(text)) if text == "g"
        ));
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('F'))),
            Some(Action::ToggleFavorite)
        ));
    }

    #[test]
    fn provider_search_backspace_edits_filter() {
        let mut s = state_with();
        s.setup.focus = Pane::Providers;
        s.setup.provider_filter = "grok".into();
        assert!(matches!(
            dispatch(&s, key(KeyCode::Backspace)),
            Some(Action::SearchProviders(text)) if text == "gro"
        ));
    }

    #[test]
    fn q_filters_on_reasoning_map() {
        let mut s = state_with();
        s.screen = crate::action::Screen::Reasoning;
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('q'))),
            Some(Action::MapFilter(text)) if text == "q"
        ));
        // Uppercase D is still the cycle-default binding.
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('D'))),
            Some(Action::CycleModelDefault)
        ));
    }

    #[test]
    fn task_pane_keys() {
        let mut s = state_with();
        s.setup.focus = Pane::Task;
        // Up/Down cycle the selected task prompt — same direction as every
        // other pane (Up back, Down forward); Ctrl+Enter starts the run.
        assert!(matches!(
            dispatch(&s, key(KeyCode::Up)),
            Some(Action::CycleTaskPrompt(-1))
        ));
        assert!(matches!(
            dispatch(&s, key(KeyCode::Down)),
            Some(Action::CycleTaskPrompt(1))
        ));
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('d'))),
            Some(Action::SetDefaultTaskPrompt)
        ));
        assert!(matches!(
            dispatch(&s, KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            Some(Action::StartRun)
        ));
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('x'))),
            Some(Action::BulkStart)
        ));
        // Free-text editing is gone: letters do nothing on the Task pane.
        assert!(dispatch(&s, key(KeyCode::Char('a'))).is_none());
    }
}
