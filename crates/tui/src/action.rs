//! Actions (reducible events) and effects (side effects the loop runs).

use crate::state::Pane;
use crate::UiMsg;

/// Everything the UI can do. `State::reduce` folds these; the returned
/// [`Effect`]s are executed by the event loop.
#[derive(Clone)]
pub enum Action {
    /// Request quit (graceful: cancels running runs first).
    Quit,
    /// Force quit even with a run still winding down.
    ForceQuit,
    SwitchScreen(Screen),
    OpenPalette,
    /// `?` — open the keybinding help overlay.
    OpenHelp,
    CloseModal,
    /// A transient status message from a background task.
    Notice(String),

    // ---- setup -----------------------------------------------------------
    CycleFocus(bool),
    /// Mouse click inside a setup pane.
    FocusPane(Pane),
    MoveSelection(i32),
    /// Live provider search text (auto-filter on typing).
    SearchProviders(String),
    ClearSearch,
    ToggleFavorite,
    /// `Enter` on the focused provider: connect flow or key modal.
    ConnectProvider,
    /// `Enter` on the focused model.
    SelectModel,
    ToggleMultiSelect,
    /// `Space` in the models pane: toggle bulk membership.
    ToggleBulk,
    ClearBulk,
    CycleReasoning(i32),
    CyclePrompt(i32),
    SetDefaultPrompt,
    /// `↑`/`↓` on the Task pane: cycle the selected task prompt.
    CycleTaskPrompt(i32),
    /// `d` on the Task pane: pin the selected task prompt as the config
    /// default.
    SetDefaultTaskPrompt,
    /// Bracketed-paste text: inserted into the focused input field.
    Paste(String),
    /// `Ctrl+Enter` on Task: run the current single selection.
    StartRun,
    /// `x` on Task: bulk-launch every selected (provider, model) pair.
    BulkStart,
    ConfirmBulkStart,

    // ---- key-entry modal ---------------------------------------------------
    EnterKeyChar(char),
    EnterKeyBackspace,
    /// Forward-delete at the cursor in the key-entry modal.
    EnterKeyDelete,
    /// Move the key-entry cursor (`±1` per arrow press).
    EnterKeyCursor(i32),
    SaveKey,

    // ---- run -------------------------------------------------------------
    NextSession,
    PrevSession,
    ScrollTranscript(i32),
    CancelSession,
    CancelAllRuns,
    RerunSession,
    ToggleRawFeed,
    OpenRunDetail,

    // ---- history ---------------------------------------------------------
    MoveHistory(i32),
    RescanHistory,
    OpenHistoryDetail,

    // ---- reasoning map ----------------------------------------------------
    MapFilter(String),
    MapClear,
    MapMove(i32),
    /// `d` on the map: cycle the default reasoning for the selected model.
    CycleModelDefault,
    /// `d` in the setup Reasoning pane: pin the current level as the
    /// model's default.
    SetModelDefault,
    /// F5 in the map: reload the Models.dev snapshot.
    ReloadSnapshot,
    /// Scroll the history-detail modal (`↑`/`↓`, mouse wheel).
    ScrollHistoryDetail(i32),
    /// Mouse wheel over a setup pane: focus it and move its selection.
    MouseWheelSetup {
        pane: Pane,
        dir: i32,
    },
    /// Mouse click on a list row: focus (when applicable) and select it.
    MouseSelectRow {
        target: SelectTarget,
        idx: usize,
    },

    // ---- palette ---------------------------------------------------------
    PaletteChar(char),
    PaletteBackspace,
    PaletteMove(i32),
    PaletteEnter,
    PaletteRunAction(usize),

    // ---- effect results --------------------------------------------------
    UiMsg(UiMsg),

    // ---- misc ------------------------------------------------------------
    RefreshModels(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Setup,
    Run,
    History,
    Reasoning,
}

impl Screen {
    pub const ALL: [Screen; 4] = [
        Screen::Setup,
        Screen::Run,
        Screen::History,
        Screen::Reasoning,
    ];
    pub fn title(&self) -> &'static str {
        match self {
            Self::Setup => "[1] Setup",
            Self::Run => "[2] Run",
            Self::History => "[3] History",
            Self::Reasoning => "[4] Reasoning",
        }
    }
    pub fn cycle(&self, forward: bool) -> Self {
        let idx = Self::ALL.iter().position(|s| s == self).unwrap_or(0);
        let next = if forward {
            (idx + 1) % Self::ALL.len()
        } else {
            (idx + Self::ALL.len() - 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

/// Which list a mouse click targeted (for row selection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectTarget {
    Providers,
    Models,
    Sessions,
    History,
    Map,
}

/// Side effects returned by `reduce`; executed by the event loop.
pub enum Effect {
    /// Spawn a Models.dev resolution for one provider (stale-guarded by id).
    FetchModels {
        provider: std::sync::Arc<dyn lmhub_core::Provider>,
        force: bool,
    },
    /// Spawn one agent run. The loop builds the `RunSpec` from the session
    /// stored under `run_id` and tags every event with that id.
    LaunchRun { run_id: u64 },
    /// Rescan the history directory (fast, runs inline).
    ScanHistory,
    /// Load the full Models.dev snapshot for the reasoning map.
    LoadSnapshot,
    /// Persist UI prefs (favorites, last selections).
    SavePrefs,
    /// Run the GitHub Copilot device-flow (long-running; results arrive as
    /// `UiMsg::Notice`).
    RunCopilotFlow,
    /// Open a directory in the platform file manager (blocking spawn).
    OpenOutputDir(std::path::PathBuf),
}
