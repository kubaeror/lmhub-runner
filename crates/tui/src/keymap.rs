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
        return Some(Action::Quit);
    }

    if let Some(modal) = &state.modal {
        return modal_keys(modal, key);
    }

    match key.code {
        KeyCode::Char('q') => return Some(Action::Quit),
        KeyCode::Char(':') => return Some(Action::OpenPalette),
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

/// Keys while a modal is open: the modal owns the input stream.
fn modal_keys(modal: &Modal, key: KeyEvent) -> Option<Action> {
    match modal {
        Modal::EnterKey { .. } => match key.code {
            KeyCode::Esc => Some(Action::CloseModal),
            KeyCode::Enter => Some(Action::SaveKey),
            KeyCode::Backspace => Some(Action::EnterKeyBackspace),
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
        Modal::HistoryDetail(_) => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => Some(Action::CloseModal),
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
            KeyCode::Up => Some(Action::TaskRecall(1)),
            KeyCode::Down => Some(Action::TaskRecall(-1)),
            KeyCode::Enter => Some(Action::StartRun),
            KeyCode::Backspace => Some(Action::TaskBackspace),
            KeyCode::Char('x') => Some(Action::BulkStart),
            KeyCode::Char(c) => Some(Action::TaskChar(c)),
            _ => None,
        },
    }
}

/// Auto-filter: typing in the providers pane edits the search text — but
/// reserved keys keep their action.
fn provider_search_char(state: &State, c: char) -> Option<Action> {
    let mut text = state.setup.provider_filter.clone();
    text.push(c);
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
        KeyCode::Char(c) => Some(Action::MapFilter(format!("{}{}", state.map.filter, c))),
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
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(lmhub_core::AuthStore::load(
            dir.path().join("auth.json"),
        )));
        let (registry, _) =
            lmhub_providers::build_registry(dir.path(), std::sync::Arc::clone(&store));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        State::new(
            registry,
            std::sync::Arc::new(lmhub_modelsdev::ModelsDevClient::new(
                dir.path().join("cache"),
                std::time::Duration::from_secs(60),
            )),
            store,
            lmhub_sandbox::SandboxRuntime::Legacy,
            lmhub_core::AppConfig::default(),
            dir.path().join("config.toml"),
            Vec::new(),
            dir.path().join("output"),
            tx,
        )
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
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('q'))),
            Some(Action::Quit)
        ));
        s.modal = Some(Modal::EnterKey {
            provider_id: "x".into(),
            input: String::new(),
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
    fn task_pane_keys() {
        let mut s = state_with();
        s.setup.focus = Pane::Task;
        assert!(matches!(
            dispatch(&s, key(KeyCode::Enter)),
            Some(Action::StartRun)
        ));
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('x'))),
            Some(Action::BulkStart)
        ));
        assert!(matches!(
            dispatch(&s, key(KeyCode::Char('a'))),
            Some(Action::TaskChar('a'))
        ));
    }
}
