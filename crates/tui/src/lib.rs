//! Interactive terminal UI for lmhub-runner.
//!
//! Three tabs:
//! - **Setup** — provider → models (auto-fetched, source shown) → model,
//!   reasoning level, system prompt (configurable files + default), task
//!   input; starting a run switches to Run;
//! - **Run** — live events feed plus tokens/cache/cost/tool-call counters;
//! - **History** — browse previous runs' `statistics.json`.

mod app;
mod ui;

use app::{App, Focus};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
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

pub enum UiMsg {
    ModelsReady(
        /// Provider id the request was made for (stale-response guard).
        Box<String>,
        Box<lmhub_core::ModelCatalog>,
        Option<Arc<lmhub_modelsdev::CatalogSnapshot>>,
    ),
    RunEvent(lmhub_core::RunEvent),
    /// Terminal state of one background run.
    RunFinished(Result<Box<lmhub_agent::RunOutcome>, String>),
    /// Transient status line (connect flows etc.).
    Notice(String),
}

pub struct TuiContext {
    pub registry: ProviderRegistry,
    pub modelsdev: Arc<lmhub_modelsdev::ModelsDevClient>,
    pub config: AppConfig,
    pub config_path: PathBuf,
    pub prompts: Vec<PromptFile>,
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
    execute!(stdout, EnterAlternateScreen).context("entering alternate screen failed")?;
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

    let mut app = App::new(
        ctx.registry,
        ctx.modelsdev,
        ctx.config,
        ctx.config_path,
        ctx.prompts,
        ctx.output_base,
        ctx.auth_store,
        ctx.sandbox_runtime,
        ui_tx,
    );
    // Auto-load models for the initially selected provider.
    app.request_models(false);

    let mut tick = tokio::time::interval(TICK_INTERVAL);

    let result = loop {
        terminal.draw(|f| ui::draw(f, &mut app))?;
        tokio::select! {
            _ = tick.tick() => {}
            ev = key_rx.recv() => match ev {
                Some(event) => handle_event(&mut app, event),
                None => break Ok(()),
            },
            msg = ui_rx.recv() => match msg {
                Some(m) => app.handle_ui_msg(m),
                None => break Ok(()),
            },
        }
        if app.should_quit {
            break Ok(());
        }
    };

    // Force-quit with a run still winding down: give a cancelled run a short
    // grace period to write statistics.json before the tokio runtime is
    // dropped (dropping it would kill the agent task mid-write).
    let unfinished_run = app
        .run
        .as_ref()
        .map(|r| r.finished_line.is_none())
        .unwrap_or(false);
    if unfinished_run {
        let grace = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(msg) = ui_rx.recv().await {
                if matches!(&msg, UiMsg::RunFinished(_)) {
                    app.handle_ui_msg(msg);
                    break;
                }
                app.handle_ui_msg(msg);
            }
        })
        .await;
        if grace.is_err() {
            tracing::warn!("force-quit: run did not finish within 5s; dropping task");
        }
    }

    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen)?;
    result
}

fn handle_event(app: &mut App, event: Event) {
    let Event::Key(key) = event else { return };
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Modal key-entry overlay captures everything.
    if matches!(app.mode, app::Mode::EnterKey { .. }) {
        match key.code {
            KeyCode::Esc => {
                app.mode = app::Mode::Normal;
                app.key_input.clear();
            }
            KeyCode::Enter => {
                app.save_entered_key();
            }
            KeyCode::Backspace => {
                app.key_input.pop();
            }
            KeyCode::Char(c) => app.key_input.push(c),
            _ => {}
        }
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q'))
    {
        quit_request(app);
        return;
    }

    match key.code {
        KeyCode::Char('q') => quit_request(app),
        KeyCode::Tab => {
            app.tab = match app.tab {
                app::Tab::Setup => app::Tab::History,
                app::Tab::History => app::Tab::Run,
                app::Tab::Run => app::Tab::Setup,
            };
            if app.tab == app::Tab::History && app.history.is_empty() {
                app.scan_history();
            }
        }
        _ => match app.tab {
            app::Tab::Setup => setup_keys(app, key.code),
            app::Tab::Run => run_keys(app, key.code),
            app::Tab::History => history_keys(app, key.code),
        },
    }

    // Model refresh: F5 force-refreshes (bypasses the Models.dev TTL); 'r'
    // reloads from cache. Neither fires while typing (provider filter or task
    // text) so typing never wipes the model selection.
    if app.tab == app::Tab::Setup
        && (matches!(key.code, KeyCode::F(5))
            || (matches!(key.code, KeyCode::Char('r'))
                && !matches!(app.focus, Focus::Providers | Focus::Task)))
    {
        app.request_models(matches!(key.code, KeyCode::F(5)));
    }
}

fn quit_request(app: &mut App) {
    if let Some(run) = &app.run {
        if run.finished_line.is_none() {
            // Cancel first so statistics.json can still be written by the agent.
            run.cancel.cancel();
            app.push_log("cancel requested — press q again to force quit");
            return;
        }
    }
    app.should_quit = true;
}

fn cycle_focus(app: &mut App, forward: bool) {
    const ORDER: [Focus; 5] = [
        Focus::Providers,
        Focus::Models,
        Focus::Reasoning,
        Focus::Prompts,
        Focus::Task,
    ];
    let idx = ORDER.iter().position(|f| *f == app.focus).unwrap_or(0);
    let next = if forward {
        (idx + 1) % ORDER.len()
    } else {
        (idx + ORDER.len() - 1) % ORDER.len()
    };
    app.focus = ORDER[next];
}

fn setup_keys(app: &mut App, code: KeyCode) {
    match app.focus {
        Focus::Providers => match code {
            KeyCode::Left | KeyCode::Right => cycle_focus(app, code == KeyCode::Right),
            KeyCode::Up => {
                let count = app.filtered_indices().len();
                if count > 0 {
                    app.provider_idx = app
                        .provider_idx
                        .saturating_sub(1)
                        .min(count.saturating_sub(1));
                    app.request_models(false);
                }
            }
            KeyCode::Down => {
                let count = app.filtered_indices().len();
                if app.provider_idx + 1 < count {
                    app.provider_idx += 1;
                    app.request_models(false);
                } else if count > 0 && app.provider_idx >= count {
                    app.provider_idx = count - 1;
                    app.request_models(false);
                }
            }
            KeyCode::Enter => app.start_connect(),
            KeyCode::Esc => app.provider_filter.clear(),
            KeyCode::Char(c) => app.provider_filter.push(c),
            _ => {}
        },
        Focus::Models => match code {
            KeyCode::Left | KeyCode::Right => cycle_focus(app, code == KeyCode::Right),
            KeyCode::Up => app.model_idx = app.model_idx.saturating_sub(1),
            KeyCode::Down if app.model_idx + 1 < app.models.len() => {
                app.model_idx += 1;
                app.reasoning_idx = 0;
            }
            _ => {}
        },
        Focus::Reasoning => {
            let levels = app.visible_reasoning_levels().len();
            match code {
                KeyCode::Left | KeyCode::Right => cycle_focus(app, code == KeyCode::Right),
                KeyCode::Up => {
                    app.reasoning_idx = app
                        .reasoning_idx
                        .saturating_sub(1)
                        .min(levels.saturating_sub(1))
                }
                KeyCode::Down if app.reasoning_idx + 1 < levels => {
                    app.reasoning_idx += 1;
                }
                _ => {}
            }
        }
        Focus::Prompts => match code {
            KeyCode::Left | KeyCode::Right => cycle_focus(app, code == KeyCode::Right),
            KeyCode::Up => app.prompt_idx = app.prompt_idx.saturating_sub(1),
            KeyCode::Down => {
                if app.prompt_idx + 1 < app.prompts.len() {
                    app.prompt_idx += 1;
                }
            }
            KeyCode::Char('d') | KeyCode::Enter => set_default_prompt(app),
            _ => {}
        },
        Focus::Task => match code {
            KeyCode::Left | KeyCode::Right => cycle_focus(app, code == KeyCode::Right),
            KeyCode::Enter => {
                if let Err(e) = app.start_run() {
                    app.push_notice(format!("✖ {e}"));
                }
            }
            KeyCode::Backspace => {
                app.task_input.pop();
            }
            KeyCode::Char(c) => app.task_input.push(c),
            _ => {}
        },
    }
}

fn set_default_prompt(app: &mut App) {
    if let Some(p) = app.prompts.get(app.prompt_idx) {
        app.config.default_prompt = Some(p.name.clone());
        match app.config.save(&app.config_path) {
            Ok(()) => app.push_log(format!("default prompt → {}", p.name)),
            Err(e) => app.push_log(format!("✖ could not save config: {e}")),
        }
    }
}

fn run_keys(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('c') => {
            if let Some(run) = &app.run {
                run.cancel.cancel();
            }
        }
        KeyCode::Up => {
            if let Some(run) = app.run.as_mut() {
                run.scroll = run.scroll.saturating_add(1);
            }
        }
        KeyCode::Down => {
            if let Some(run) = app.run.as_mut() {
                run.scroll = run.scroll.saturating_sub(1);
            }
        }
        _ => {}
    }
}

fn history_keys(app: &mut App, code: KeyCode) {
    if app.history_detail.is_some() {
        if matches!(code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
            app.history_detail = None;
        }
        return;
    }
    match code {
        KeyCode::Up => app.history_idx = app.history_idx.saturating_sub(1),
        KeyCode::Down => {
            if app.history_idx + 1 < app.history.len() {
                app.history_idx += 1;
            }
        }
        KeyCode::F(5) => app.scan_history(),
        KeyCode::Enter => {
            if let Some(row) = app.history.get(app.history_idx) {
                let path = row.path.clone();
                match std::fs::read_to_string(&path) {
                    Ok(raw) => app.history_detail = Some(raw),
                    Err(e) => {
                        app.history_detail = Some(format!("cannot read {}: {e}", path.display()))
                    }
                }
            }
        }
        _ => {}
    }
}
