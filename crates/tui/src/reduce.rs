//! `State::reduce` — the single place where actions enter the app. This file
//! is deliberately a *thin dispatcher*: it routes each [`Action`] to the
//! per-screen reducer module that owns it and returns the [`Effect`]s the
//! event loop should run.
//!
//! Reducers are "pure-ish": they mutate state and return effects, but never
//! spawn tasks or do blocking IO — those become [`Effect`]s whose results
//! come back as `Action::UiMsg`.

use crate::action::{Action, Effect};
use crate::state::State;

impl State {
    /// Single entry point for every [`Action`]. Routes to the reducer that
    /// owns the action; returns effects for the event loop to execute.
    ///
    /// The match is exhaustive: adding a new `Action` variant is a compile
    /// error here, which forces a conscious decision about which screen owns
    /// it.
    pub fn reduce(&mut self, action: Action) -> Vec<Effect> {
        match action {
            // ---- global / cross-screen -------------------------------------
            Action::Quit
            | Action::ForceQuit
            | Action::SwitchScreen(_)
            | Action::OpenPalette
            | Action::OpenHelp
            | Action::CloseModal
            | Action::Notice(_)
            | Action::UiMsg(_)
            | Action::Paste(_)
            | Action::ScrollHistoryDetail(_)
            | Action::PaletteChar(_)
            | Action::PaletteBackspace
            | Action::PaletteMove(_)
            | Action::PaletteEnter
            | Action::PaletteRunAction(_) => self.reduce_global(action),

            // ---- setup ------------------------------------------------------
            Action::CycleFocus(_)
            | Action::FocusPane(_)
            | Action::MoveSelection(_)
            | Action::SearchProviders(_)
            | Action::ClearSearch
            | Action::ToggleFavorite
            | Action::ConnectProvider
            | Action::SelectModel
            | Action::ToggleMultiSelect
            | Action::ToggleBulk
            | Action::ClearBulk
            | Action::CycleReasoning(_)
            | Action::CyclePrompt(_)
            | Action::SetDefaultPrompt
            | Action::CycleTaskPrompt(_)
            | Action::SetDefaultTaskPrompt
            | Action::StartRun
            | Action::BulkStart
            | Action::ConfirmBulkStart
            | Action::EnterKeyChar(_)
            | Action::EnterKeyBackspace
            | Action::EnterKeyDelete
            | Action::EnterKeyCursor(_)
            | Action::SaveKey
            | Action::RefreshModels(_) => self.reduce_setup(action),

            // ---- run ---------------------------------------------------------
            Action::NextSession
            | Action::PrevSession
            | Action::ScrollTranscript(_)
            | Action::CancelSession
            | Action::CancelAllRuns
            | Action::RerunSession
            | Action::ToggleRawFeed
            | Action::OpenRunDetail => self.reduce_run(action),

            // ---- history ------------------------------------------------------
            Action::MoveHistory(_) | Action::RescanHistory | Action::OpenHistoryDetail => {
                self.reduce_history(action)
            }

            // ---- reasoning map ------------------------------------------------
            Action::MapFilter(_)
            | Action::MapClear
            | Action::MapMove(_)
            | Action::CycleModelDefault
            | Action::SetModelDefault
            | Action::ReloadSnapshot => self.reduce_map(action),

            // ---- mouse row selection (routed to the owning screen) ------------
            Action::MouseSelectRow { target, .. } => match target {
                crate::action::SelectTarget::Providers | crate::action::SelectTarget::Models => {
                    self.reduce_setup(action)
                }
                crate::action::SelectTarget::Sessions => self.reduce_run(action),
                crate::action::SelectTarget::History => self.reduce_history(action),
                crate::action::SelectTarget::Map => self.reduce_map(action),
            },

            // ---- mouse wheel over a setup pane --------------------------------
            Action::MouseWheelSetup { .. } => self.reduce_setup(action),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Modal, RunSession, RunSessionStatus, SetupState};
    use crate::transcript::Transcript;
    use std::time::Instant;
    use tokio_util::sync::CancellationToken;

    fn model(id: &str) -> lmhub_core::ModelInfo {
        lmhub_core::ModelInfo {
            id: id.into(),
            name: id.into(),
            ..Default::default()
        }
    }

    /// A model that supports reasoning levels off/low/high.
    fn reasoning_model(id: &str) -> lmhub_core::ModelInfo {
        lmhub_core::ModelInfo {
            id: id.into(),
            name: id.into(),
            capabilities: lmhub_core::Capabilities {
                reasoning: true,
                reasoning_levels: Some(vec![
                    lmhub_core::ReasoningLevel::Off,
                    lmhub_core::ReasoningLevel::Low,
                    lmhub_core::ReasoningLevel::High,
                ]),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Fake a loaded catalog for `provider_id` so bulk specs resolve.
    /// Models are reasoning-capable so pinned/chosen levels survive clamping.
    fn seed_catalog(state: &mut State, provider_id: &str, model_ids: &[&str]) {
        let catalog = lmhub_core::ModelCatalog {
            models: model_ids.iter().map(|m| reasoning_model(m)).collect(),
            source: Some(lmhub_core::ModelListSource::ModelsDev),
            warnings: Vec::new(),
        };
        state.setup.catalog_cache.insert(
            provider_id.to_string(),
            crate::state::CachedCatalog::from_catalog(&catalog, None),
        );
    }

    /// Seed the task-prompt list with tempdir-backed files named `name`.
    fn seed_task_prompts(state: &mut State, names: &[&str]) {
        let dir = std::env::temp_dir().join(format!("lmhub-taskprompt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        state.task_prompts = names
            .iter()
            .map(|n| {
                let path = dir.join(format!("{n}.md"));
                let content = format!("{n} the codebase");
                if std::fs::metadata(&path).is_err() {
                    std::fs::write(&path, &content).unwrap();
                }
                crate::PromptFile {
                    name: n.to_string(),
                    path,
                }
            })
            .collect();
    }

    #[test]
    fn quit_requires_two_presses_with_running_run() {
        let (mut state, _dir) = crate::testutil::test_state();
        state.runs.runs.push(RunSession {
            id: 1,
            provider_id: "x".into(),
            model_id: "m".into(),
            reasoning: "off".into(),
            task: "t".into(),
            status: RunSessionStatus::Running,
            started: Instant::now(),
            cancel: Some(CancellationToken::new()),
            transcript: Transcript::default(),
            tokens: Default::default(),
            tool_ok: 0,
            tool_fail: 0,
            errors: 0,
            warnings: 0,
            pricing: None,
            finished_line: None,
            final_text: None,
            run_dir: None,
            scroll: 0,
            raw_feed: false,
            delta_count: 0,
            provider: state.registry.get("openai").unwrap(),
            model: model("gpt-4o"),
            reasoning_level: lmhub_core::ReasoningLevel::Off,
            system_prompt: String::new(),
            pricing_ctx: None,
        });
        assert!(!state.quit);
        let effects = state.reduce(Action::Quit);
        assert!(!state.quit, "first quit only cancels");
        assert!(state.cancel_requested);
        assert!(
            effects.iter().any(|e| matches!(e, Effect::SavePrefs)),
            "prefs persisted on quit"
        );
        state.reduce(Action::ForceQuit);
        assert!(state.quit && state.force_quit);
    }

    #[test]
    fn task_prompt_cycles_list() {
        let (mut state, _dir) = crate::testutil::test_state();
        seed_task_prompts(&mut state, &["build", "refactor"]);
        state.reduce(Action::CycleTaskPrompt(1));
        assert_eq!(state.setup.task_prompt_idx, 1);
        state.reduce(Action::CycleTaskPrompt(1));
        assert_eq!(state.setup.task_prompt_idx, 0);
        state.reduce(Action::CycleTaskPrompt(-1));
        assert_eq!(state.setup.task_prompt_idx, 1);
    }

    #[test]
    fn start_run_resolves_selected_task_prompt() {
        let (mut state, _dir) = crate::testutil::test_state();
        seed_task_prompts(&mut state, &["build", "refactor"]);
        state.setup.models = vec![model("gpt-4o")];
        state.setup.task_prompt_idx = 1;
        let effects = state.reduce(Action::StartRun);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::LaunchRun { .. })),
            "launch effect expected"
        );
        let run = state.runs.runs.last().unwrap();
        assert_eq!(run.model_id, "gpt-4o");
        assert!(run.task.contains("refactor"));
    }

    #[test]
    fn start_run_without_task_prompts_uses_builtin() {
        let (mut state, _dir) = crate::testutil::test_state();
        state.setup.models = vec![model("gpt-4o")];
        let effects = state.reduce(Action::StartRun);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::LaunchRun { .. })),
            "launch effect expected"
        );
        assert_eq!(
            state.runs.runs.last().unwrap().task,
            lmhub_core::DEFAULT_TASK_PROMPT
        );
    }

    #[test]
    fn bulk_uses_selected_task_prompt() {
        let (mut state, _dir) = crate::testutil::test_state();
        seed_task_prompts(&mut state, &["build", "refactor"]);
        seed_catalog(&mut state, "openai", &["gpt-4o", "gpt-4o-mini"]);
        state.setup.models = vec![model("gpt-4o"), model("gpt-4o-mini")];
        state.setup.bulk = std::collections::BTreeSet::from([
            ("openai".into(), "gpt-4o".into()),
            ("openai".into(), "gpt-4o-mini".into()),
        ]);
        state.setup.task_prompt_idx = 1;
        state.reduce(Action::BulkStart);
        state.reduce(Action::ConfirmBulkStart);
        assert_eq!(state.runs.runs.len(), 2);
        assert!(state.runs.runs.iter().all(|r| r.task.contains("refactor")));
    }

    #[test]
    fn paste_strips_controls_in_single_line_fields() {
        let (mut state, _dir) = crate::testutil::test_state();
        // Provider search: multi-line paste collapses to one line.
        state.setup.focus = crate::state::Pane::Providers;
        state.reduce(Action::Paste("an\nthropic".into()));
        assert_eq!(state.setup.provider_filter.as_str(), "anthropic");
        // Reasoning-map filter behaves the same (`\r` normalizes to `\n`,
        // then the newline is stripped as a control char).
        state.screen = crate::action::Screen::Reasoning;
        state.reduce(Action::Paste("gpt\ro1".into()));
        assert_eq!(state.map.filter.as_str(), "gpto1");
        // EnterKey modal: single line, no controls.
        state.modal = Some(Modal::EnterKey {
            provider_id: "x".into(),
            input: "sk-".into(),
        });
        state.reduce(Action::Paste("abc\ndef".into()));
        assert!(matches!(
            &state.modal,
            Some(Modal::EnterKey { input, .. }) if input.as_str() == "sk-abcdef"
        ));
    }

    #[test]
    fn bulk_toggle_spans_providers_and_clears() {
        let (mut state, _dir) = crate::testutil::test_state();
        seed_catalog(&mut state, "openai", &["gpt-4o", "gpt-4o-mini"]);
        state.setup.models = vec![model("gpt-4o"), model("gpt-4o-mini")];
        state.setup.model_idx = 0;
        state.reduce(Action::ToggleBulk); // auto-enables multi-select
        assert!(state.setup.multi_select);
        assert_eq!(state.setup.bulk.len(), 1);
        state.setup.model_idx = 1;
        state.reduce(Action::ToggleBulk);
        assert_eq!(state.setup.bulk.len(), 2);
        // Switch to another provider: selection survives.
        state.setup.provider_filter = "anthropic".into();
        state.reduce(Action::ClearBulk);
        assert!(state.setup.bulk.is_empty());
    }

    #[test]
    fn bulk_start_queues_beyond_concurrency_cap() {
        let (mut state, _dir) = crate::testutil::test_state();
        state.prefs.max_concurrent_runs = 2;
        seed_catalog(&mut state, "openai", &["gpt-4o", "gpt-4o-mini", "gpt-4.1"]);
        state.setup.models = vec![model("gpt-4o"), model("gpt-4o-mini"), model("gpt-4.1")];
        seed_task_prompts(&mut state, &["build"]);
        state.setup.bulk = std::collections::BTreeSet::from([
            ("openai".into(), "gpt-4o".into()),
            ("openai".into(), "gpt-4o-mini".into()),
            ("openai".into(), "gpt-4.1".into()),
        ]);
        let _effects = state.reduce(Action::BulkStart);
        assert!(matches!(state.modal, Some(Modal::BulkConfirm)));
        // Confirm: 2 launch immediately, 1 queues.
        let effects = state.reduce(Action::ConfirmBulkStart);
        let launches: Vec<&Effect> = effects
            .iter()
            .filter(|e| matches!(e, Effect::LaunchRun { .. }))
            .collect();
        assert_eq!(launches.len(), 2, "cap 2 → two launches");
        let statuses: Vec<RunSessionStatus> = state.runs.runs.iter().map(|r| r.status).collect();
        assert_eq!(
            statuses,
            vec![
                RunSessionStatus::Running,
                RunSessionStatus::Running,
                RunSessionStatus::Pending
            ]
        );
        assert!(state.setup.bulk.is_empty(), "bulk cleared after launch");
        // A finish frees a slot → the pending run promotes.
        let effects = state.reduce(Action::UiMsg(crate::UiMsg::RunFinished {
            run_id: 1,
            result: Err("cancelled".into()),
        }));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::LaunchRun { run_id: 3 })),
            "third run promotes when a slot frees"
        );
        assert_eq!(state.runs.runs[2].status, RunSessionStatus::Running);
    }

    #[test]
    fn favorites_toggle_and_persist_flag() {
        let (mut state, _dir) = crate::testutil::test_state();
        let provider_id = state.selected_provider().unwrap().id().to_string();
        let effects = state.reduce(Action::ToggleFavorite);
        assert!(state.prefs.favorites.contains(&provider_id));
        assert!(effects.iter().any(|e| matches!(e, Effect::SavePrefs)));
        state.reduce(Action::ToggleFavorite);
        assert!(!state.prefs.favorites.contains(&provider_id));
    }

    #[test]
    fn search_ranks_and_filters() {
        let (mut state, _dir) = crate::testutil::test_state();
        state.reduce(Action::SearchProviders("groq".into()));
        let rows = state.provider_rows();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.group.is_none()), "search is flat");
        assert_eq!(rows[0].registry_idx, {
            let all = state.registry.all();
            all.iter().position(|p| p.id() == "groq").unwrap()
        });
        state.reduce(Action::ClearSearch);
        let rows = state.provider_rows();
        assert!(rows.iter().any(|r| r.group.is_some()), "browse has headers");
    }

    #[test]
    fn setup_state_defaults_are_safe() {
        let s = SetupState::default();
        assert_eq!(s.focus, crate::state::Pane::Providers);
        assert_eq!(s.provider_idx, 0);
        assert!(s.bulk.is_empty());
    }

    #[test]
    fn model_default_reasoning_snaps_on_selection() {
        let (mut state, _dir) = crate::testutil::test_state();
        // Two models with distinct reasoning sets.
        let m1 = lmhub_core::ModelInfo {
            id: "claude-3-7-sonnet".into(),
            capabilities: lmhub_core::Capabilities {
                reasoning: true,
                reasoning_levels: Some(vec![
                    lmhub_core::ReasoningLevel::Off,
                    lmhub_core::ReasoningLevel::Low,
                    lmhub_core::ReasoningLevel::High,
                ]),
                ..Default::default()
            },
            ..Default::default()
        };
        state.setup.models = vec![m1.clone()];
        state.setup.model_idx = 0;
        // Cycle up to high (records the choice in `setup.reasoning`).
        state.reduce(Action::CycleReasoning(2));
        assert_eq!(state.selected_reasoning(), lmhub_core::ReasoningLevel::High);
        let effects = state.reduce(Action::SetModelDefault);
        assert_eq!(
            state.prefs.model_defaults.get("claude-3-7-sonnet"),
            Some(&lmhub_core::ReasoningLevel::High)
        );
        assert!(effects.iter().any(|e| matches!(e, Effect::SavePrefs)));
        // Moving away and back snaps to the default.
        state.setup.reasoning_idx = 0;
        state.snap_reasoning_to_default();
        assert_eq!(state.setup.reasoning_idx, 2);
        assert_eq!(state.selected_reasoning(), lmhub_core::ReasoningLevel::High);
        let _ = m1;
    }

    #[test]
    fn reasoning_choice_survives_model_switch() {
        let (mut state, _dir) = crate::testutil::test_state();
        state.setup.models = vec![reasoning_model("gpt-4o"), reasoning_model("gpt-4o-mini")];
        state.setup.model_idx = 0;
        state.setup.focus = crate::state::Pane::Models;
        state.reduce(Action::CycleReasoning(2)); // high
        assert_eq!(state.selected_reasoning(), lmhub_core::ReasoningLevel::High);
        // Navigating to another model must keep the chosen level.
        state.reduce(Action::MoveSelection(1));
        assert_eq!(
            state.setup.reasoning_idx, 2,
            "display follows the kept level"
        );
        assert_eq!(state.selected_reasoning(), lmhub_core::ReasoningLevel::High);
    }

    #[test]
    fn bulk_run_uses_chosen_reasoning_after_navigation() {
        let (mut state, _dir) = crate::testutil::test_state();
        seed_catalog(&mut state, "openai", &["gpt-4o", "gpt-4o-mini"]);
        state.setup.models = vec![reasoning_model("gpt-4o"), reasoning_model("gpt-4o-mini")];
        seed_task_prompts(&mut state, &["build"]);
        state.setup.bulk = std::collections::BTreeSet::from([
            ("openai".into(), "gpt-4o".into()),
            ("openai".into(), "gpt-4o-mini".into()),
        ]);
        state.setup.focus = crate::state::Pane::Models;
        state.reduce(Action::CycleReasoning(2)); // high
                                                 // Land on a different model: the old bug degraded the bulk fallback
                                                 // to off here (reasoning was derived from the *current* model).
        state.reduce(Action::MoveSelection(1));
        state.reduce(Action::BulkStart);
        state.reduce(Action::ConfirmBulkStart);
        assert!(
            state.runs.runs.iter().all(|r| r.reasoning == "high"),
            "chosen reasoning must reach every bulk run: {:?}",
            state
                .runs
                .runs
                .iter()
                .map(|r| (r.model_id.as_str(), r.reasoning.as_str()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn bulk_run_clamps_to_model_capabilities() {
        let (mut state, _dir) = crate::testutil::test_state();
        seed_catalog(&mut state, "openai", &["gpt-4o", "gpt-4o-mini"]);
        // Override mini's catalog entry with a non-reasoning model.
        let mut cache = state.setup.catalog_cache.get("openai").unwrap().clone();
        cache.models[1] = model("gpt-4o-mini");
        state.setup.catalog_cache.insert("openai".into(), cache);
        state.setup.models = vec![reasoning_model("gpt-4o"), reasoning_model("gpt-4o-mini")];
        seed_task_prompts(&mut state, &["build"]);
        state.setup.bulk = std::collections::BTreeSet::from([
            ("openai".into(), "gpt-4o".into()),
            ("openai".into(), "gpt-4o-mini".into()),
        ]);
        state.reduce(Action::CycleReasoning(2)); // high
        state.reduce(Action::BulkStart);
        state.reduce(Action::ConfirmBulkStart);
        let by_model: std::collections::BTreeMap<String, String> = state
            .runs
            .runs
            .iter()
            .map(|r| (r.model_id.clone(), r.reasoning.clone()))
            .collect();
        // Reasoning model keeps the choice; the non-reasoning one clamps.
        assert_eq!(by_model["gpt-4o"], "high");
        assert_eq!(by_model["gpt-4o-mini"], "off");
    }

    #[test]
    fn bulk_run_prefers_pinned_default_over_chosen_level() {
        let (mut state, _dir) = crate::testutil::test_state();
        seed_catalog(&mut state, "openai", &["gpt-4o", "gpt-4o-mini"]);
        state.setup.models = vec![reasoning_model("gpt-4o"), reasoning_model("gpt-4o-mini")];
        seed_task_prompts(&mut state, &["build"]);
        state
            .prefs
            .model_defaults
            .insert("gpt-4o-mini".into(), lmhub_core::ReasoningLevel::Low);
        state.setup.bulk = std::collections::BTreeSet::from([
            ("openai".into(), "gpt-4o".into()),
            ("openai".into(), "gpt-4o-mini".into()),
        ]);
        state.reduce(Action::CycleReasoning(2)); // high chosen
        state.reduce(Action::BulkStart);
        state.reduce(Action::ConfirmBulkStart);
        let by_model: std::collections::BTreeMap<String, String> = state
            .runs
            .runs
            .iter()
            .map(|r| (r.model_id.clone(), r.reasoning.clone()))
            .collect();
        assert_eq!(by_model["gpt-4o"], "high", "chosen level applies");
        assert_eq!(by_model["gpt-4o-mini"], "low", "pinned default wins");
    }

    #[test]
    fn map_cycle_default_wraps_through_supported_levels() {
        let (mut state, _dir) = crate::testutil::test_state();
        state.snapshot_all = Some(std::sync::Arc::new(lmhub_modelsdev::CatalogSnapshot {
            catalog: lmhub_modelsdev::catalog::Catalog {
                providers: std::collections::BTreeMap::from([(
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
                            serde_json::from_value(serde_json::json!({
                                "id": "claude-3-7-sonnet",
                                "reasoning": true,
                                "reasoning_options": [{ "type": "effort", "values": ["off", "low", "high"] }],
                            }))
                            .unwrap(),
                        )]),
                    },
                )]),
            },
            fetched_at: "t".into(),
            version: "v".into(),
            stale: false,
        }));
        state.map.idx = 0;
        // First d: skip off → low.
        state.reduce(Action::CycleModelDefault);
        assert_eq!(
            state.prefs.model_defaults.get("claude-3-7-sonnet"),
            Some(&lmhub_core::ReasoningLevel::Low)
        );
        // Second d: high; third d: wraps through off → off.
        state.reduce(Action::CycleModelDefault);
        assert_eq!(
            state.prefs.model_defaults.get("claude-3-7-sonnet"),
            Some(&lmhub_core::ReasoningLevel::High)
        );
        state.reduce(Action::CycleModelDefault);
        assert_eq!(
            state.prefs.model_defaults.get("claude-3-7-sonnet"),
            Some(&lmhub_core::ReasoningLevel::Off)
        );
        // And one more lands on low again.
        state.reduce(Action::CycleModelDefault);
        assert_eq!(
            state.prefs.model_defaults.get("claude-3-7-sonnet"),
            Some(&lmhub_core::ReasoningLevel::Low)
        );
    }

    #[test]
    fn bulk_uses_per_model_default_reasoning() {
        let (mut state, _dir) = crate::testutil::test_state();
        seed_catalog(&mut state, "openai", &["gpt-4o", "gpt-4o-mini"]);
        // Both models must actually support reasoning for a pinned level to
        // survive clamping.
        let reasoning_model = |id: &str| lmhub_core::ModelInfo {
            id: id.into(),
            capabilities: lmhub_core::Capabilities {
                reasoning: true,
                reasoning_levels: Some(vec![
                    lmhub_core::ReasoningLevel::Off,
                    lmhub_core::ReasoningLevel::Low,
                    lmhub_core::ReasoningLevel::High,
                ]),
                ..Default::default()
            },
            ..Default::default()
        };
        state.setup.models = vec![reasoning_model("gpt-4o"), reasoning_model("gpt-4o-mini")];
        seed_task_prompts(&mut state, &["build"]);
        state
            .prefs
            .model_defaults
            .insert("gpt-4o-mini".into(), lmhub_core::ReasoningLevel::Low);
        state.setup.bulk = std::collections::BTreeSet::from([
            ("openai".into(), "gpt-4o".into()),
            ("openai".into(), "gpt-4o-mini".into()),
        ]);
        state.reduce(Action::BulkStart);
        state.reduce(Action::ConfirmBulkStart);
        let by_model: std::collections::BTreeMap<String, String> = state
            .runs
            .runs
            .iter()
            .map(|r| (r.model_id.clone(), r.reasoning.clone()))
            .collect();
        // No default → chosen level (off by default); default set → low.
        assert_eq!(by_model["gpt-4o"], "off");
        assert_eq!(by_model["gpt-4o-mini"], "low");
    }
}
