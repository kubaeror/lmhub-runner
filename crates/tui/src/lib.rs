//! Interactive terminal UI for lmhub-runner.
//!
//! Elm-style core: [`State`] + [`Action`]s + pure-ish `reduce` returning
//! [`Effect`]s, which the loop here executes (async fetches/launches come
//! back as `UiMsg` actions).
//!
//! Screens:
//! - **Setup** — searchable/grouped provider list, model picker with
//!   multi-select (bulk start across providers), reasoning, system + task
//!   prompt pickers;
//! - **Run** — multiple concurrent sessions, structured transcript, stats;
//! - **History** — previous runs' `statistics.json` (pretty-printed detail).
//!
//! `:` opens the command palette. Mouse clicks focus panes / switch tabs.

mod action;
mod history;
mod keymap;
mod pricing;
mod provider_search;
mod reasoning_map;
mod reduce;
mod state;
mod transcript;
mod view;

pub use action::Action;
pub use keymap::dispatch;
pub use state::State;

use action::Effect;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyEvent, KeyEventKind, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use lmhub_core::AppConfig;
use lmhub_providers::ProviderRegistry;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub struct PromptFile {
    pub name: String,
    pub path: PathBuf,
}

/// Messages from background tasks (models fetch, agent runs, connect flows).
#[derive(Clone)]
pub enum UiMsg {
    ModelsReady {
        requested_for: String,
        catalog: Box<lmhub_core::ModelCatalog>,
        snapshot: Option<Arc<lmhub_modelsdev::CatalogSnapshot>>,
    },
    RunEvent {
        run_id: u64,
        event: lmhub_core::RunEvent,
    },
    /// Terminal state of one background run.
    RunFinished {
        run_id: u64,
        result: Result<Box<lmhub_agent::RunOutcome>, String>,
    },
    /// Full Models.dev snapshot for the reasoning map.
    SnapshotLoaded(Arc<lmhub_modelsdev::CatalogSnapshot>),
    /// Transient status line (connect flows etc.).
    Notice(String),
}

pub struct TuiContext {
    pub registry: ProviderRegistry,
    pub modelsdev: Arc<lmhub_modelsdev::ModelsDevClient>,
    pub config: AppConfig,
    pub config_path: PathBuf,
    pub prompts: Vec<PromptFile>,
    pub task_prompts: Vec<PromptFile>,
    pub output_base: PathBuf,
    pub auth_store: Arc<std::sync::Mutex<lmhub_core::AuthStore>>,
    pub sandbox_runtime: lmhub_sandbox::SandboxRuntime,
}

/// UI tick: how often the screen redraws even without input/events.
const TICK_INTERVAL: Duration = Duration::from_millis(200);

pub async fn run_tui(ctx: TuiContext) -> anyhow::Result<()> {
    use anyhow::Context as _;
    enable_raw_mode().context("TUI requires an interactive terminal (run inside a real tty)")?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        // Bracketed paste: pasted text arrives as a single `Event::Paste`
        // instead of a burst of key events (which would trigger app-level
        // bindings like `q` quit or `x` bulk-run from pasted content).
        EnableBracketedPaste
    )
    .context("entering alternate screen failed")?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend).context("terminal backend init failed")?;

    // UI channel is owned by the app so handlers can spawn async work.
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();
    let (key_tx, mut key_rx) = mpsc::channel::<Event>(128);

    // Blocking input reader on its own thread; forwards raw events.
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if key_tx.blocking_send(ev).is_err() {
                break;
            }
        }
    });

    let mut state = State::new(
        ctx.registry,
        ctx.modelsdev,
        ctx.auth_store,
        ctx.sandbox_runtime,
        ctx.config,
        ctx.config_path,
        ctx.prompts,
        ctx.task_prompts,
        ctx.output_base,
        ui_tx,
    );
    // Auto-load models for the initially selected provider.
    let initial_effects = state.request_models(false);
    run_effects(&mut state, initial_effects);

    let mut tick = tokio::time::interval(TICK_INTERVAL);

    let result = loop {
        terminal.draw(|f| view::draw(f, &mut state))?;
        tokio::select! {
            _ = tick.tick() => {}
            ev = key_rx.recv() => match ev {
                Some(event) => handle_input(&mut state, event),
                None => break Ok(()),
            },
            msg = ui_rx.recv() => match msg {
                Some(m) => {
                    let effects = state.reduce(Action::UiMsg(m));
                    run_effects(&mut state, effects);
                }
                None => break Ok(()),
            },
        }
        if state.quit {
            break Ok(());
        }
    };

    // Force-quit with runs still winding down: give cancelled runs a short
    // grace period to write statistics.json before the tokio runtime is
    // dropped (dropping it would kill the agent tasks mid-write).
    let unfinished = state
        .runs
        .runs
        .iter()
        .filter(|r| r.status != state::RunSessionStatus::Finished)
        .count();
    if unfinished > 0 {
        let grace = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let still = state
                    .runs
                    .runs
                    .iter()
                    .filter(|r| r.status != state::RunSessionStatus::Finished)
                    .count();
                if still == 0 {
                    break;
                }
                match ui_rx.recv().await {
                    Some(msg) => {
                        let mut effects = state.reduce(Action::UiMsg(msg));
                        // Never start queued runs while quitting.
                        effects.retain(|e| !matches!(e, Effect::LaunchRun { .. }));
                        run_effects(&mut state, effects);
                    }
                    None => break,
                }
            }
        })
        .await;
        if grace.is_err() {
            tracing::warn!("force-quit: runs did not finish within 5s; dropping tasks");
        }
    }

    state.prefs.save(&state.prefs_path);
    execute!(std::io::stdout(), DisableMouseCapture)?;
    disable_raw_mode()?;
    execute!(
        std::io::stdout(),
        LeaveAlternateScreen,
        DisableBracketedPaste
    )?;
    result
}

fn handle_input(state: &mut State, event: Event) {
    let action = match event {
        Event::Key(key) => key_action(state, key),
        Event::Paste(text) => Some(Action::Paste(text)),
        Event::Mouse(m) => match m.kind {
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                view::mouse_action(state, m.column, m.row)
            }
            _ => None,
        },
        _ => None,
    };
    if let Some(action) = action {
        let effects = state.reduce(action);
        run_effects(state, effects);
    }
}

fn key_action(state: &mut State, key: KeyEvent) -> Option<Action> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    // Quit handling (incl. the q-vs-search decision) lives in
    // `keymap::dispatch` so it stays in one place.
    crate::dispatch(state, key)
}

/// Execute effects: spawn async work (results come back as UiMsg), run
/// fast inline work (history scan, prefs persistence).
fn run_effects(state: &mut State, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::FetchModels { provider, force } => {
                let provider = Arc::clone(&provider);
                let mdc = Arc::clone(&state.modelsdev);
                let tx = state.ui_tx.clone();
                let provider_id = provider.id().to_string();
                tokio::spawn(async move {
                    if force {
                        let _ = mdc.refresh().await; // fall back to stale cache below
                    }
                    let catalog =
                        lmhub_providers::resolve_model_catalog(provider.as_ref(), &mdc).await;
                    let snapshot = match mdc.load().await {
                        Ok(s) => Some(Arc::new(s)),
                        Err(e) => {
                            let _ = tx.send(UiMsg::Notice(format!(
                                "models.dev catalog unavailable: {e}"
                            )));
                            None
                        }
                    };
                    let _ = tx.send(UiMsg::ModelsReady {
                        requested_for: provider_id,
                        catalog: Box::new(catalog),
                        snapshot,
                    });
                });
            }
            Effect::LaunchRun { run_id } => {
                if let Some(spec) = build_agent_spec(state, run_id) {
                    spawn_run(state.ui_tx.clone(), run_id, spec);
                }
            }
            Effect::ScanHistory => {
                state.history.rows = crate::history::scan_history(&state.output_base);
                state.history.idx = state
                    .history
                    .idx
                    .min(state.history.rows.len().saturating_sub(1));
            }
            Effect::LoadSnapshot => {
                let mdc = Arc::clone(&state.modelsdev);
                let tx = state.ui_tx.clone();
                tokio::spawn(async move {
                    match mdc.load().await {
                        Ok(snapshot) => {
                            let _ = tx.send(UiMsg::SnapshotLoaded(Arc::new(snapshot)));
                        }
                        Err(e) => {
                            let _ = tx.send(UiMsg::Notice(format!(
                                "models.dev catalog unavailable: {e}"
                            )));
                        }
                    }
                });
            }
            Effect::SavePrefs => state.prefs.save(&state.prefs_path),
        }
    }
}

/// Assemble the agent `RunSpec` for a session from state + config.
fn build_agent_spec(state: &State, run_id: u64) -> Option<lmhub_agent::RunSpec> {
    let run = state.runs.find(run_id)?;
    let cancel = run.cancel.clone()?;
    Some(lmhub_agent::RunSpec {
        provider: run.provider.clone(),
        family_override: run.model.family.clone(),
        model: run.model.clone(),
        reasoning: run.reasoning_level,
        system_prompt: run.system_prompt.clone(),
        task: run.task.clone(),
        output_base: state.output_base.clone(),
        pricing: run.pricing_ctx.clone(),
        enable_prompt_cache: true,
        max_turns: state.config.max_turns,
        max_output_tokens: state.config.max_output_tokens,
        deadline: state.config.run_timeout(),
        cancel,
        sandbox: lmhub_sandbox::SandboxConfig {
            allowed_commands: state.config.allowed_commands.clone(),
            command_timeout: state.config.command_timeout(),
            read_file_max_bytes: state.config.read_file_max_bytes,
            write_file_max_bytes: state.config.write_file_max_bytes,
            runtime: state.sandbox_runtime.clone(),
        },
    })
}

/// Bridge one agent run into tagged UI messages.
fn spawn_run(ui_tx: mpsc::UnboundedSender<UiMsg>, run_id: u64, spec: lmhub_agent::RunSpec) {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<lmhub_core::RunEvent>();
    let app_tx = ui_tx.clone();
    tokio::spawn(async move {
        let handle = tokio::spawn(lmhub_agent::execute(spec, Some(event_tx)));
        // Bridge agent events into UI messages, tagged with the run id.
        while let Some(event) = event_rx.recv().await {
            let _ = app_tx.send(UiMsg::RunEvent { run_id, event });
        }
        let outcome = handle.await;
        let result = match outcome {
            Ok(Ok(out)) => Ok(Box::new(out)),
            Ok(Err(e)) => Err(format!("run failed: {e}")),
            Err(join_err) => Err(format!("task panicked: {join_err}")),
        };
        let _ = app_tx.send(UiMsg::RunFinished { run_id, result });
    });
}
