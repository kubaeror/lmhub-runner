//! Golden snapshot tests: render every screen and modal into a fixed
//! `TestBackend` buffer and compare the plain-text dump against committed
//! files in `tests/goldens/*.txt`.
//!
//! Regenerate after intentional layout/copy changes with:
//! `LMHUB_UPDATE_GOLDENS=1 cargo test -p lmhub-tui goldens`

use crate::state::{CachedCatalog, Modal, RunSession, RunSessionStatus, State};
use crate::transcript::{ToolCallEvent, Transcript, Turn};
use crate::PromptFile;
use lmhub_core::{
    Capabilities, ModelCatalog, ModelInfo, ModelListSource, ModelPricing, ReasoningLevel, Usage,
};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const WIDTH: u16 = 100;
const HEIGHT: u16 = 28;

fn render(state: &State) -> String {
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut layout = crate::view::RenderInfo::default();
    let frame = terminal
        .draw(|f| layout = crate::view::draw(f, state))
        .unwrap();
    let buf = frame.buffer;
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        if y + 1 < buf.area.height {
            out.push('\n');
        }
    }
    out
}

fn check_golden(name: &str, state: &State) {
    let actual = render(state);
    let path = format!("{}/tests/goldens/{name}.txt", env!("CARGO_MANIFEST_DIR"));
    if std::env::var_os("LMHUB_UPDATE_GOLDENS").is_some() {
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing golden {path} — run with LMHUB_UPDATE_GOLDENS=1 to create it")
    });
    if actual != expected {
        let a: Vec<&str> = actual.lines().collect();
        let e: Vec<&str> = expected.lines().collect();
        let mut msg = format!("golden mismatch: {name}\n");
        for (i, (ea, ee)) in a.iter().zip(e.iter()).enumerate() {
            if ea != ee {
                msg.push_str(&format!(
                    "first difference at line {i}:\n  expected: {ee:?}\n  actual:   {ea:?}\n"
                ));
                break;
            }
        }
        if a.len() != e.len() {
            msg.push_str(&format!(
                "line count: expected {} actual {}\n",
                e.len(),
                a.len()
            ));
        }
        panic!("{msg}\n--- expected ---\n{expected}\n--- actual ---\n{actual}");
    }
}

// ---- seeds ---------------------------------------------------------------

fn model(id: &str, name: &str, reasoning: bool) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        name: name.into(),
        family: Some(
            if id.starts_with("gpt") {
                "GPT"
            } else {
                "Claude"
            }
            .into(),
        ),
        context_window: Some(128_000),
        max_output: Some(16_384),
        capabilities: Capabilities {
            tool_call: true,
            reasoning,
            prompt_caching: true,
            reasoning_levels: if reasoning {
                Some(vec![
                    ReasoningLevel::Off,
                    ReasoningLevel::Low,
                    ReasoningLevel::High,
                ])
            } else {
                None
            },
        },
    }
}

fn seed_openai(state: &mut State) {
    let catalog = ModelCatalog {
        models: vec![
            model("gpt-4o", "GPT-4o", true),
            model("gpt-4o-mini", "GPT-4o mini", true),
            model("gpt-4.1", "GPT-4.1", true),
        ],
        source: Some(ModelListSource::ModelsDev),
        warnings: vec!["stale cache served offline".into()],
    };
    let cache = CachedCatalog::from_catalog(&catalog, None);
    state
        .setup
        .catalog_cache
        .insert("openai".into(), cache.clone());
    state.setup.provider_idx = state
        .provider_rows()
        .iter()
        .filter(|r| r.group.is_none())
        .position(|r| {
            r.registry_idx
                == state
                    .registry
                    .all()
                    .iter()
                    .position(|p| p.id() == "openai")
                    .unwrap()
        })
        .unwrap();
    state.adopt_catalog(&cache);
    state
        .prefs
        .model_defaults
        .insert("gpt-4o".into(), ReasoningLevel::High);
}

fn seed_prompts(state: &mut State) {
    state.prompts = vec![
        PromptFile {
            name: "default".into(),
            path: PathBuf::new(),
        },
        PromptFile {
            name: "expert".into(),
            path: PathBuf::new(),
        },
    ];
    state.task_prompts = vec![
        PromptFile {
            name: "build".into(),
            path: PathBuf::new(),
        },
        PromptFile {
            name: "refactor".into(),
            path: PathBuf::new(),
        },
    ];
    state.setup.task_prompt_idx = 1;
}

fn session(id: u64, model_id: &str, status: RunSessionStatus, state: &State) -> RunSession {
    RunSession {
        id,
        provider_id: "openai".into(),
        model_id: model_id.into(),
        reasoning: "high".into(),
        task: "refactor the codebase".into(),
        status,
        started: Instant::now(),
        cancel: None,
        transcript: Transcript::default(),
        tokens: Default::default(),
        tool_ok: 0,
        tool_fail: 0,
        errors: 0,
        warnings: 0,
        pricing: Some(ModelPricing {
            input_per_million_usd: 3.0,
            output_per_million_usd: 15.0,
            cache_read_per_million_usd: Some(0.3),
            cache_write_per_million_usd: Some(3.75),
        }),
        finished_line: None,
        final_text: None,
        run_dir: None,
        scroll: 0,
        raw_feed: false,
        delta_count: 0,
        provider: state.registry.get("openai").unwrap(),
        model: model(model_id, model_id, true),
        reasoning_level: ReasoningLevel::High,
        system_prompt: String::new(),
        pricing_ctx: None,
    }
}

fn finished_run(state: &State) -> RunSession {
    let mut run = session(1, "gpt-4o", RunSessionStatus::Finished, state);
    run.finished_line = Some(
        "■ completed — cost 0.001234 USD — statistics: /tmp/out/GPT/gpt-4o/high/2026-08-24T00-00-00-12345678/statistics.json".into(),
    );
    run.run_dir = Some(PathBuf::from(
        "/tmp/out/GPT/gpt-4o/high/2026-08-24T00-00-00-12345678",
    ));
    run.final_text = Some("Done. All tests pass and the build is green.".into());
    run.tokens = Usage {
        input_tokens: 4218,
        output_tokens: 8930,
        reasoning_tokens: Some(200),
        cache_read_tokens: Some(1800),
        cache_write_tokens: Some(10),
    };
    run.tool_ok = 4;
    run.tool_fail = 1;
    run.errors = 1;
    run.warnings = 2;
    run.transcript.turns = vec![
        Turn {
            number: 1,
            llm_text:
                "I'll inspect the project structure first.\nThen I can decide where the changes go."
                    .into(),
            tool_calls: vec![
                ToolCallEvent {
                    name: "list_directory".into(),
                    status: "success".into(),
                    duration_ms: 12,
                    error: None,
                },
                ToolCallEvent {
                    name: "read_file".into(),
                    status: "success".into(),
                    duration_ms: 4,
                    error: None,
                },
            ],
            usage: Usage {
                input_tokens: 900,
                output_tokens: 120,
                ..Default::default()
            },
            duration_ms: 1200,
            stop_reason: "tool_call".into(),
        },
        Turn {
            number: 2,
            llm_text: "Found the failing test. Applying the fix and running the suite…".into(),
            tool_calls: vec![
                ToolCallEvent {
                    name: "run_command".into(),
                    status: "success".into(),
                    duration_ms: 2400,
                    error: None,
                },
                ToolCallEvent {
                    name: "run_command".into(),
                    status: "error".into(),
                    duration_ms: 900,
                    error: Some("exit code 1: 3 tests failed".into()),
                },
            ],
            usage: Usage {
                input_tokens: 1600,
                output_tokens: 480,
                ..Default::default()
            },
            duration_ms: 3400,
            stop_reason: "end_turn".into(),
        },
    ];
    run
}

fn running_run(state: &State) -> RunSession {
    let mut run = session(2, "gpt-4o-mini", RunSessionStatus::Running, state);
    run.delta_count = 57;
    run.transcript.turns = vec![Turn {
        number: 1,
        llm_text: "Streaming a long answer about the refactor plan…".into(),
        ..Default::default()
    }];
    run.tokens = Usage {
        input_tokens: 300,
        output_tokens: 57,
        ..Default::default()
    };
    run.cancel = Some(tokio_util::sync::CancellationToken::new());
    run
}

fn snapshot() -> lmhub_modelsdev::CatalogSnapshot {
    let entry = |id: &str, reasoning: bool, options: serde_json::Value| {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "reasoning": reasoning,
            "reasoning_options": options,
        }))
        .unwrap()
    };
    lmhub_modelsdev::CatalogSnapshot {
        catalog: lmhub_modelsdev::catalog::Catalog {
            providers: std::collections::BTreeMap::from([
                (
                    "anthropic".into(),
                    lmhub_modelsdev::catalog::ProviderEntry {
                        id: "anthropic".into(),
                        name: "Anthropic".into(),
                        env: vec![],
                        api: None,
                        doc: None,
                        npm: None,
                        models: std::collections::BTreeMap::from([(
                            "claude-3-7-sonnet".into(),
                            entry(
                                "claude-3-7-sonnet",
                                true,
                                serde_json::json!([{ "type": "effort", "values": ["off", "low", "high"] }]),
                            ),
                        )]),
                    },
                ),
                (
                    "openai".into(),
                    lmhub_modelsdev::catalog::ProviderEntry {
                        id: "openai".into(),
                        name: "OpenAI".into(),
                        env: vec![],
                        api: None,
                        doc: None,
                        npm: None,
                        models: std::collections::BTreeMap::from([
                            (
                                "gpt-4o".into(),
                                entry(
                                    "gpt-4o",
                                    true,
                                    serde_json::json!([{ "type": "effort", "values": ["off", "low", "high"] }]),
                                ),
                            ),
                            (
                                "gpt-4.1".into(),
                                entry(
                                    "gpt-4.1",
                                    true,
                                    serde_json::json!([{ "type": "effort", "values": ["low", "high"] }]),
                                ),
                            ),
                        ]),
                    },
                ),
            ]),
        },
        fetched_at: "2026-08-24T00:00:00Z".into(),
        version: "abc123".into(),
        stale: false,
    }
}

// ---- screens -------------------------------------------------------------

#[test]
fn goldens_setup_screen() {
    let (mut state, _dir) = crate::testutil::test_state();
    seed_openai(&mut state);
    seed_prompts(&mut state);
    state.prefs.favorites.insert("anthropic".into());
    state.setup.multi_select = true;
    state.setup.bulk.insert(("openai".into(), "gpt-4o".into()));
    check_golden("setup", &state);
}

#[test]
fn goldens_setup_search() {
    let (mut state, _dir) = crate::testutil::test_state();
    seed_openai(&mut state);
    seed_prompts(&mut state);
    state.setup.provider_filter = "grok".into();
    state.setup.provider_idx = 0;
    check_golden("setup_search", &state);
}

#[test]
fn goldens_run_finished() {
    let (mut state, _dir) = crate::testutil::test_state();
    state.screen = crate::action::Screen::Run;
    state.runs.runs = vec![finished_run(&state), running_run(&state)];
    state.runs.selected = 0;
    check_golden("run_finished", &state);
}

#[test]
fn goldens_run_running() {
    let (mut state, _dir) = crate::testutil::test_state();
    state.screen = crate::action::Screen::Run;
    state.runs.runs = vec![finished_run(&state), running_run(&state)];
    state.runs.selected = 1;
    check_golden("run_running", &state);
}

#[test]
fn goldens_run_raw_feed() {
    let (mut state, _dir) = crate::testutil::test_state();
    state.screen = crate::action::Screen::Run;
    let mut run = running_run(&state);
    run.raw_feed = true;
    run.transcript.feed = vec![
        "— turn 1".into(),
        "✔ run_command  (1200 ms)".into(),
        "⚠ cache price unknown for route".into(),
        "✘ tool run_command failed: exit 1".into(),
        "■ run finished".into(),
    ];
    state.runs.runs = vec![run];
    check_golden("run_raw_feed", &state);
}

#[test]
fn goldens_history() {
    let (mut state, _dir) = crate::testutil::test_state();
    state.screen = crate::action::Screen::History;
    let row = |model: &str, status: &str, usd: Option<f64>| crate::history::HistoryRow {
        path: PathBuf::from("/tmp/out/GPT/gpt-4o/high/x/statistics.json"),
        family: "GPT".into(),
        model: model.into(),
        reasoning: "high".into(),
        status: status.into(),
        duration_ms: Some(60_000),
        total_tokens: Some(12_000),
        total_usd: usd,
        started_at: Some("2026-08-24T00:00:00Z".into()),
    };
    state.history.rows = vec![
        row("gpt-4o", "completed", Some(0.001234)),
        row("gpt-4o-mini", "cancelled", None),
        row("gpt-4.1", "timeout", Some(0.5)),
    ];
    state.history.idx = 0;
    check_golden("history", &state);
}

#[test]
fn goldens_reasoning_map() {
    let (mut state, _dir) = crate::testutil::test_state();
    state.screen = crate::action::Screen::Reasoning;
    state.snapshot_all = Some(Arc::new(snapshot()));
    state
        .prefs
        .model_defaults
        .insert("claude-3-7-sonnet".into(), ReasoningLevel::High);
    state.map.filter = "sonnet".into();
    check_golden("reasoning_map", &state);
}

// ---- modals --------------------------------------------------------------

#[test]
fn goldens_modal_palette() {
    let (mut state, _dir) = crate::testutil::test_state();
    seed_openai(&mut state);
    seed_prompts(&mut state);
    state.modal = Some(Modal::Palette {
        filter: "ru".into(),
        cursor: 0,
    });
    check_golden("modal_palette", &state);
}

#[test]
fn goldens_modal_key_entry() {
    let (mut state, _dir) = crate::testutil::test_state();
    seed_openai(&mut state);
    state.modal = Some(Modal::EnterKey {
        provider_id: "openai".into(),
        input: "sk-abcdefghij".into(),
    });
    check_golden("modal_key_entry", &state);
}

#[test]
fn goldens_modal_bulk_confirm() {
    let (mut state, _dir) = crate::testutil::test_state();
    seed_openai(&mut state);
    seed_prompts(&mut state);
    state.setup.bulk = std::collections::BTreeSet::from([
        ("openai".into(), "gpt-4o".into()),
        ("openai".into(), "gpt-4o-mini".into()),
    ]);
    state.modal = Some(Modal::BulkConfirm);
    check_golden("modal_bulk_confirm", &state);
}

#[test]
fn goldens_modal_run_detail() {
    let (mut state, _dir) = crate::testutil::test_state();
    state.screen = crate::action::Screen::Run;
    let run = finished_run(&state);
    state.runs.runs = vec![run];
    state.modal = Some(Modal::RunDetail { run_id: 1 });
    check_golden("modal_run_detail", &state);
}

#[test]
fn goldens_modal_history_detail() {
    let (mut state, _dir) = crate::testutil::test_state();
    state.screen = crate::action::Screen::History;
    state.history.rows = vec![crate::history::HistoryRow {
        path: PathBuf::from("/tmp/out/GPT/gpt-4o/high/x/statistics.json"),
        family: "GPT".into(),
        model: "gpt-4o".into(),
        reasoning: "high".into(),
        status: "completed".into(),
        duration_ms: Some(60_000),
        total_tokens: Some(12_000),
        total_usd: Some(0.001234),
        started_at: Some("2026-08-24T00:00:00Z".into()),
    }];
    let detail = [
        "run      : abc123",
        "status   : completed",
        "provider : openai (native-openai)",
        "family   : GPT",
        "model    : gpt-4o",
        "reasoning: high",
        "started  : 2026-08-24T00:00:00Z",
        "finished : 2026-08-24T00:01:00Z",
        "duration : 60000 ms",
        "",
        "— tokens —",
        "input 1000/output 200",
        "reasoning 50 · cache-read 400 · cache-write 0",
        "total 1200 · cache-hit 0.4000",
    ]
    .join("\n");
    state.modal = Some(Modal::HistoryDetail(detail));
    check_golden("modal_history_detail", &state);
}

// ---- reducer-level effect tests -------------------------------------------

#[test]
fn copilot_connect_returns_flow_effect() {
    let (mut state, _dir) = crate::testutil::test_state();
    state.setup.provider_filter = "github-copilot".into();
    state.setup.provider_idx = 0;
    let effects = state.reduce(crate::action::Action::ConnectProvider);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, crate::action::Effect::RunCopilotFlow)),
        "copilot connect must defer to an effect, not spawn in reduce"
    );
    assert!(state.modal.is_none(), "no key modal for the device flow");
}

#[test]
fn palette_open_output_dir_returns_effect() {
    let (mut state, _dir) = crate::testutil::test_state();
    state.modal = Some(Modal::Palette {
        filter: "open".into(),
        cursor: 0,
    });
    let effects = state.reduce(crate::action::Action::PaletteEnter);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, crate::action::Effect::OpenOutputDir(_))),
        "open-dir must be an effect, not a blocking spawn in reduce"
    );
    assert!(state.modal.is_none(), "palette closes on execute");
}

#[test]
fn start_run_persists_prefs_exactly_once() {
    let (mut state, _dir) = crate::testutil::test_state();
    state.setup.models = vec![lmhub_core::ModelInfo::bare("gpt-4o")];
    let effects = state.reduce(crate::action::Action::StartRun);
    let saves = effects
        .iter()
        .filter(|e| matches!(e, crate::action::Effect::SavePrefs))
        .count();
    assert_eq!(saves, 1, "one SavePrefs effect per mutating action");
    // capture_prefs snapped the current provider into prefs (registry
    // order — the first provider, whatever that is).
    assert!(state.prefs.last_provider.is_some());
}

#[test]
fn enter_key_modal_edits_at_cursor() {
    let (mut state, _dir) = crate::testutil::test_state();
    state.modal = Some(Modal::EnterKey {
        provider_id: "openai".into(),
        input: "sk-12345678".into(),
    });
    state.reduce(crate::action::Action::EnterKeyCursor(-4));
    state.reduce(crate::action::Action::EnterKeyChar('X'));
    let Some(Modal::EnterKey { input, .. }) = &state.modal else {
        panic!("modal gone");
    };
    assert_eq!(input.as_str(), "sk-1234X5678");
    state.reduce(crate::action::Action::EnterKeyCursor(1));
    state.reduce(crate::action::Action::EnterKeyDelete);
    let Some(Modal::EnterKey { input, .. }) = &state.modal else {
        panic!("modal gone");
    };
    // Delete removes the char *at* the cursor ("5"), not before it.
    assert_eq!(input.as_str(), "sk-1234X578");
}
