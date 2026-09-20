//! End-to-end agent-loop tests against a mock provider (no network).
//!
//! Verifies the run contract: directory layout, events.jsonl contents,
//! and that statistics.json is written for success AND provider failures.

use lmhub_agent::{execute, RunSpec};
use lmhub_core::{
    ChatRequest, ChatResponse, ModelInfo, Provider, ProviderCaps, ReasoningLevel, Role, StopReason,
    ToolCallRequest, Usage,
};
use std::sync::Arc;
use std::time::Duration;

struct MockProvider {
    fail_on_first_call: bool,
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Mock"
    }
    fn provider_type(&self) -> &str {
        "mock-native"
    }
    fn api_key_env(&self) -> &str {
        "MOCK_API_KEY"
    }
    fn models_dev_hint(&self) -> Option<&str> {
        None
    }
    fn supports_model_listing(&self) -> bool {
        false
    }
    fn caps(&self) -> ProviderCaps {
        ProviderCaps {
            tool_calls: true,
            reasoning: true,
            prompt_caching: true,
        }
    }
    async fn list_models_api(&self) -> lmhub_core::Result<Option<Vec<String>>> {
        Ok(None)
    }

    async fn chat(&self, request: &ChatRequest) -> lmhub_core::Result<ChatResponse> {
        if self.fail_on_first_call {
            return Err(lmhub_core::CoreError::Provider("503 exploded".into()));
        }
        let used_tool = request.messages.iter().any(|m| m.role == Role::Tool);
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 10,
            reasoning_tokens: Some(4),
            cache_read_tokens: Some(40),
            cache_write_tokens: None,
        };
        Ok(if used_tool {
            ChatResponse {
                text: "all done".into(),
                thinking: None,
                tool_calls: vec![],
                usage,
                stop_reason: StopReason::EndTurn,
                raw_assistant_message: None,
                warnings: vec![],
                duration_ms: 5,
            }
        } else {
            ChatResponse {
                text: String::new(),
                thinking: None,
                tool_calls: vec![ToolCallRequest {
                    id: "call_1".into(),
                    name: "list_directory".into(),
                    arguments: serde_json::json!({"path": "."}),
                }],
                usage,
                stop_reason: StopReason::ToolUse,
                raw_assistant_message: None,
                warnings: vec![],
                duration_ms: 7,
            }
        })
    }
}

/// Mock that keeps issuing a large-output tool call for `tool_turns`
/// requests, then answers. Records the estimated size of every request it
/// receives so tests can assert the context budget was respected.
struct TurnProvider {
    tool_turns: usize,
    calls: std::sync::Mutex<usize>,
    request_sizes: Arc<std::sync::Mutex<Vec<u64>>>,
}

#[async_trait::async_trait]
impl Provider for TurnProvider {
    fn id(&self) -> &str {
        "turn-mock"
    }
    fn display_name(&self) -> &str {
        "Turn Mock"
    }
    fn provider_type(&self) -> &str {
        "mock-native"
    }
    fn api_key_env(&self) -> &str {
        "MOCK_API_KEY"
    }
    fn models_dev_hint(&self) -> Option<&str> {
        None
    }
    fn supports_model_listing(&self) -> bool {
        false
    }
    fn caps(&self) -> ProviderCaps {
        ProviderCaps {
            tool_calls: true,
            reasoning: false,
            prompt_caching: false,
        }
    }
    async fn list_models_api(&self) -> lmhub_core::Result<Option<Vec<String>>> {
        Ok(None)
    }

    async fn chat(&self, request: &ChatRequest) -> lmhub_core::Result<ChatResponse> {
        self.request_sizes
            .lock()
            .unwrap()
            .push(lmhub_agent::estimate_request_tokens(
                &request.system,
                &request.tools,
                &request.messages,
                request.max_tokens as u64,
            ));
        let mut n = self.calls.lock().unwrap();
        *n += 1;
        let usage = Usage {
            input_tokens: 50,
            output_tokens: 5,
            ..Default::default()
        };
        if *n <= self.tool_turns {
            Ok(ChatResponse {
                text: String::new(),
                thinking: None,
                tool_calls: vec![ToolCallRequest {
                    id: format!("call_{n}"),
                    name: "run_command".into(),
                    arguments: serde_json::json!({
                        "argv": ["node", "-e", "console.log('x'.repeat(4000))"]
                    }),
                }],
                usage,
                stop_reason: StopReason::ToolUse,
                raw_assistant_message: None,
                warnings: vec![],
                duration_ms: 3,
            })
        } else {
            Ok(ChatResponse {
                text: "done".into(),
                thinking: None,
                tool_calls: vec![],
                usage,
                stop_reason: StopReason::EndTurn,
                raw_assistant_message: None,
                warnings: vec![],
                duration_ms: 3,
            })
        }
    }
}

fn spec(base: &std::path::Path, provider: Arc<dyn Provider>) -> RunSpec {
    RunSpec {
        provider,
        model: ModelInfo {
            id: "mock-1".into(),
            name: "Mock One".into(),
            family: Some("MockFam".into()),
            context_window: Some(8192),
            max_output: Some(1024),
            capabilities: lmhub_core::Capabilities {
                tool_call: true,
                reasoning: true,
                prompt_caching: true,
                reasoning_levels: None,
            },
        },
        family_override: None,
        reasoning: ReasoningLevel::High,
        system_prompt: "behave".into(),
        task: "make something".into(),
        output_base: base.to_path_buf(),
        pricing: None,
        enable_prompt_cache: true,
        max_turns: 5,
        max_output_tokens: 512,
        max_history_tokens: None,
        tool_result_max_bytes: 8_192,
        deadline: Duration::from_secs(30),
        cancel: tokio_util::sync::CancellationToken::new(),
        sandbox: lmhub_sandbox::SandboxConfig {
            allowed_commands: vec!["node".into()],
            command_timeout: Duration::from_secs(30),
            read_file_max_bytes: 4096,
            write_file_max_bytes: 1_048_576,
            runtime: lmhub_sandbox::SandboxRuntime::Legacy,
            allow_legacy_commands: true,
        },
    }
}

#[tokio::test]
async fn happy_path_writes_full_output_structure() {
    let tmp = tempfile::tempdir().unwrap();
    let outcome = execute(
        spec(
            tmp.path(),
            Arc::new(MockProvider {
                fail_on_first_call: false,
            }),
        ),
        None,
    )
    .await
    .unwrap();

    // layout: output/MockFam/mock-1/high/{start}-{runid8}/model-output/
    let route = tmp.path().join("MockFam").join("mock-1").join("high");
    assert!(outcome.run_dir.starts_with(&route));
    let run_component = outcome
        .run_dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(run_component.contains('-'), "run dir: {run_component}");
    assert!(outcome.workspace_dir.is_dir());
    assert!(outcome.run_dir.join("statistics.json").is_file());
    assert!(outcome.run_dir.join("events.jsonl").is_file());
    assert!(outcome.run_dir.join("errors.log").is_file());

    let stats: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(outcome.run_dir.join("statistics.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stats["status"], "completed");
    assert_eq!(stats["provider"], "mock");
    assert_eq!(stats["providerType"], "mock-native");
    assert_eq!(stats["family"], "MockFam");
    assert_eq!(stats["model"], "mock-1");
    assert_eq!(stats["reasoning"], "high");
    assert_eq!(stats["tokens"]["cacheHitRatio"], 0.4); // 40/100
    assert_eq!(stats["tokens"]["reasoning"], 8); // two turns × 4
    assert_eq!(stats["toolCalls"]["total"], 1);
    assert_eq!(stats["toolCalls"]["successRatio"], 1.0);
    assert_eq!(stats["pricing"]["totalUsd"], serde_json::Value::Null);
    // statistics.json runId must be traceable to its directory suffix.
    let run_id = stats["runId"].as_str().unwrap();
    assert!(
        run_component.ends_with(&run_id[..8]),
        "dir {run_component} does not end with {run_id}"
    );
    // performance block is populated (two LLM turns, durations recorded).
    assert_eq!(stats["performance"]["llmRequests"], 2);
    assert_eq!(stats["performance"]["turns"], 2);
    assert_eq!(stats["performance"]["maxLlmRequestMs"], 7);

    // events.jsonl: parseable JSONL with a successful tool call event
    let events_raw = std::fs::read_to_string(outcome.run_dir.join("events.jsonl")).unwrap();
    let types: Vec<String> = events_raw
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(types.contains(&"run_started".into()));
    assert!(types.contains(&"tool_call".into()));
    assert!(types.contains(&"run_finished".into()));
    assert!(events_raw.contains("\"status\":\"success\""));
    // no secrets ever
    assert!(!events_raw.contains("sk-secret"));
}

#[tokio::test]
async fn provider_failure_still_writes_statistics() {
    let tmp = tempfile::tempdir().unwrap();
    let outcome = execute(
        spec(
            tmp.path(),
            Arc::new(MockProvider {
                fail_on_first_call: true,
            }),
        ),
        None,
    )
    .await
    .unwrap();

    let stats: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(outcome.run_dir.join("statistics.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stats["status"], "error");
    assert_eq!(stats["errors"]["count"], 1);
    assert_eq!(stats["errors"]["logPath"], "errors.log");

    let errors = std::fs::read_to_string(outcome.run_dir.join("errors.log")).unwrap();
    assert!(errors.contains("provider_api"), "{errors}");
}

/// Pricing warnings land in `warningsCount` (emitted before the counter
/// snapshot) and events.jsonl agrees with the document; `run_finished` is
/// persisted before `statistics.json` (ordering invariant).
#[tokio::test]
async fn pricing_warnings_are_counted_in_statistics() {
    let tmp = tempfile::tempdir().unwrap();
    let outcome = execute(
        spec(
            tmp.path(),
            Arc::new(MockProvider {
                fail_on_first_call: false,
            }),
        ),
        None,
    )
    .await
    .unwrap();
    assert!(outcome.persistence_error.is_none());

    let events_raw = std::fs::read_to_string(outcome.run_dir.join("events.jsonl")).unwrap();
    let warnings = events_raw
        .lines()
        .filter(|l| l.contains("\"type\":\"warning\""))
        .count();
    assert!(warnings >= 1, "expected the no-price warning: {events_raw}");

    let stats: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(outcome.run_dir.join("statistics.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        stats["warningsCount"].as_u64().unwrap() as usize,
        warnings,
        "warningsCount must equal the warning events (pricing warnings included)"
    );
    assert_eq!(stats["pricing"]["totalUsd"], serde_json::Value::Null);
    assert!(events_raw.contains("run_finished"));
}

/// 40 tool turns against an 8k-window model: every provider call must fit
/// the window, the budgeter must have trimmed/dropped history (warnings in
/// events.jsonl), and the run must end in a sensible state.
#[tokio::test]
async fn context_budget_bounds_every_request_on_a_long_run() {
    let tmp = tempfile::tempdir().unwrap();
    let sizes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = Arc::new(TurnProvider {
        tool_turns: 40,
        calls: std::sync::Mutex::new(0),
        request_sizes: Arc::clone(&sizes),
    });
    let mut run_spec = spec(tmp.path(), provider);
    run_spec.max_turns = 45; // enough for 40 tool turns + the final answer
    run_spec.tool_result_max_bytes = 2_048; // force trimming as well as dropping

    let window = run_spec.model.context_window.unwrap();
    let outcome = execute(run_spec, None).await.unwrap();
    assert!(outcome.persistence_error.is_none());

    assert!(
        matches!(
            outcome.stats.status,
            lmhub_core::RunStatus::Completed | lmhub_core::RunStatus::LimitExceeded
        ),
        "sensible terminal status, got {:?}",
        outcome.stats.status
    );

    let sizes = sizes.lock().unwrap();
    assert!(
        sizes.len() >= 2,
        "provider should have been called repeatedly"
    );
    for (i, size) in sizes.iter().enumerate() {
        assert!(
            *size <= window,
            "request {i} estimated {size} tokens > window {window}"
        );
    }

    let events = std::fs::read_to_string(outcome.run_dir.join("events.jsonl")).unwrap();
    assert!(
        events.contains("truncated an old tool result") || events.contains("dropped"),
        "budgeter must record what it trimmed: {events}"
    );
}

#[tokio::test]
async fn consecutive_runs_never_share_a_run_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let mk = |fail: bool| {
        execute(
            spec(
                tmp.path(),
                Arc::new(MockProvider {
                    fail_on_first_call: fail,
                }),
            ),
            None,
        )
    };
    let first = mk(false).await.unwrap();
    let second = mk(false).await.unwrap();
    assert_ne!(first.run_dir, second.run_dir, "run dirs must be unique");
    // The shared route dir contains both runs; neither clobbers the other.
    assert_eq!(
        first.run_dir.parent(),
        Some(
            tmp.path()
                .join("MockFam")
                .join("mock-1")
                .join("high")
                .as_path()
        )
    );
    assert!(first.run_dir.join("statistics.json").is_file());
    assert!(second.run_dir.join("statistics.json").is_file());
    // Workspaces are fresh per run (the dirty-workspace bug is gone).
    assert!(!first.workspace_dir.join("marker.txt").exists());
    std::fs::write(first.workspace_dir.join("marker.txt"), "stale").unwrap();
    assert!(!second.workspace_dir.join("marker.txt").exists());
}
