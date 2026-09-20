//! The agent loop: messages → provider → tool calls (sandboxed) → …
//!
//! Guarantees:
//! - `statistics.json` is written for **every** terminal state
//!   (completed / error / timeout / cancelled / limit_exceeded, even panics);
//! - every tool call lands in `events.jsonl` with status + duration;
//! - every failure also lands in `errors.log`;
//! - the model can only act through the sandboxed [`ToolRuntime`].

use crate::pricing as cost;
use crate::sink::EventSink;
use futures::FutureExt;
use lmhub_core::stats::{build_document, RunIdentity, RunMetrics, RunStatus};
use lmhub_core::{
    infer_family, now_ts, ChatMessage, ChatRequest, CoreError, ModelInfo, PricingContext, Provider,
    ReasoningLevel, RunEvent,
};
use lmhub_sandbox::{tool_specs, SandboxConfig, ToolRuntime};
use serde_json::json;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub struct RunSpec {
    pub provider: Arc<dyn Provider>,
    pub model: ModelInfo,
    /// Explicit family override (from config/Models.dev); heuristic fallback.
    pub family_override: Option<String>,
    pub reasoning: ReasoningLevel,
    pub system_prompt: String,
    pub task: String,
    /// Root `output/` directory.
    pub output_base: PathBuf,
    pub pricing: Option<PricingContext>,
    pub enable_prompt_cache: bool,
    pub max_turns: u32,
    pub max_output_tokens: u32,
    /// Operator cap on history tokens (AppConfig `max_history_tokens`);
    /// `None` derives the budget from the model context window.
    pub max_history_tokens: Option<u64>,
    /// Tool results larger than this are trimmed (oldest first) when the
    /// context budget is exceeded (AppConfig `tool_result_max_bytes`).
    pub tool_result_max_bytes: u64,
    pub deadline: Duration,
    pub cancel: CancellationToken,
    pub sandbox: SandboxConfig,
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub stats: lmhub_core::StatisticsDocument,
    pub run_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub final_text: Option<String>,
    /// `Some` when a terminal artifact could not be persisted (finalize or
    /// `statistics.json` write failed). The run still returns its outcome; the
    /// caller must surface this instead of reporting silent success.
    pub persistence_error: Option<String>,
}

enum LoopExit {
    Completed(String),
    TurnLimitReached,
}

/// Execute one run end-to-end. Only catastrophic IO failures return `Err`.
pub async fn execute(
    spec: RunSpec,
    ui_tx: Option<tokio::sync::mpsc::UnboundedSender<RunEvent>>,
) -> anyhow::Result<RunOutcome> {
    let started_wall = Instant::now();
    // Family priority: explicit override > model metadata > id heuristic.
    let family = match spec.family_override.as_deref().map(str::trim) {
        Some(f) if !f.is_empty() => f.to_string(),
        _ => infer_family(&spec.model.id, spec.model.family.as_ref()),
    };
    // Models declare their supported reasoning levels (models.dev
    // `reasoning_options`); clamp before anything is built or named so an
    // unsupported level is never sent upstream.
    let requested_reasoning = spec.reasoning;
    let reasoning =
        requested_reasoning.clamp_to(spec.model.capabilities.reasoning_levels.as_deref());
    let reasoning_str = reasoning.to_string();

    // Every run gets its own directory so reruns never clobber artifacts or
    // reuse a dirty workspace: output/{family}/{model}/{reasoning}/{start}-{id8}/
    let run_id = lmhub_core::stats::gen_run_id();
    let started_dir = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let run_dir = spec
        .output_base
        .join(lmhub_core::family::sanitize_component(&family))
        .join(lmhub_core::family::sanitize_component(&spec.model.id))
        .join(lmhub_core::family::sanitize_component(&reasoning_str))
        .join(format!("{started_dir}-{}", &run_id[..8]));
    let workspace_dir = run_dir.join("model-output");

    std::fs::create_dir_all(&workspace_dir)?;
    let sink = EventSink::create(&run_dir, ui_tx)?;

    if reasoning != requested_reasoning {
        sink.warning(&format!(
            "reasoning level {requested_reasoning} not supported by model {} — using {reasoning}",
            spec.model.id
        ));
    }

    let identity = RunIdentity {
        provider: spec.provider.id().to_string(),
        provider_type: spec.provider.provider_type().to_string(),
        family: family.clone(),
        model: spec.model.id.clone(),
        reasoning: reasoning_str.clone(),
        started_at: now_ts(),
    };

    let provider_caps = spec.provider.caps();
    let cache_supported = provider_caps.prompt_caching && spec.model.capabilities.prompt_caching;
    let mut metrics = RunMetrics::new(spec.enable_prompt_cache, cache_supported);

    sink.emit(&RunEvent::RunStarted {
        ts: now_ts(),
        provider: identity.provider.clone(),
        provider_type: identity.provider_type.clone(),
        family: identity.family.clone(),
        model: identity.model.clone(),
        reasoning: identity.reasoning.clone(),
        task_chars: spec.task.chars().count() as u64,
    });

    let tool_rt = ToolRuntime::create(&workspace_dir, spec.sandbox.clone())?;

    // ---- agent loop with cancel/deadline/panic guards ---------------------
    let loop_result =
        AssertUnwindSafe(run_loop(&spec, &sink, &mut metrics, &tool_rt, reasoning)).catch_unwind();

    let exit: Result<Result<LoopExit, CoreError>, String> = tokio::select! {
        biased;
        _ = spec.cancel.cancelled() => Ok(Err(CoreError::Cancelled)),
        r = tokio::time::timeout(spec.deadline, loop_result) => match r {
            Ok(Ok(Ok(loop_exit))) => Ok(Ok(loop_exit)),
            Ok(Ok(Err(e))) => Ok(Err(e)),
            Ok(Err(payload)) => Err(panic_message(&payload)),
            Err(_elapsed) => Ok(Err(CoreError::Timeout)),
        },
    };

    let (status, final_text): (RunStatus, Option<String>) = match exit {
        Ok(Ok(LoopExit::Completed(text))) => (RunStatus::Completed, Some(text)),
        Ok(Ok(LoopExit::TurnLimitReached)) => {
            sink.error(
                "limit_exceeded",
                &format!(
                    "max turns reached without a final answer (model={}, reasoning={})",
                    spec.model.id, reasoning_str
                ),
            );
            (RunStatus::LimitExceeded, None)
        }
        Ok(Err(CoreError::Cancelled)) => (RunStatus::Cancelled, None),
        Ok(Err(CoreError::Timeout)) => {
            sink.error(
                "timeout",
                &format!(
                    "run exceeded its wall-clock deadline ({}s, model={}, reasoning={})",
                    spec.deadline.as_secs(),
                    spec.model.id,
                    reasoning_str
                ),
            );
            (RunStatus::Timeout, None)
        }
        Ok(Err(e @ CoreError::LimitExceeded(_))) => {
            // Budget refusal is a distinct terminal state, not a provider
            // error: the operator can act on it (smaller task / bigger model).
            sink.core_error(
                &e,
                &format!(
                    "context budget refused the request (model={}, reasoning={})",
                    spec.model.id, reasoning_str
                ),
            );
            (RunStatus::LimitExceeded, None)
        }
        Ok(Err(e)) => {
            sink.core_error(
                &e,
                &format!(
                    "agent run failed (model={}, reasoning={})",
                    spec.model.id, reasoning_str
                ),
            );
            (RunStatus::Error, None)
        }
        Err(panic_msg) => {
            sink.error(
                "panic",
                &format!(
                    "agent loop panicked (model={}, reasoning={}): {panic_msg}",
                    spec.model.id, reasoning_str
                ),
            );
            (RunStatus::Error, None)
        }
    };

    // Make user cancellation visible beyond the terminal status line.
    if status == RunStatus::Cancelled {
        sink.warning("run cancelled by user");
    }

    let finished_at = now_ts();
    let wall = started_wall.elapsed();

    // ---- pricing (before the counter snapshot: pricing warnings count) ----
    let pricing_outcome = cost::compute(spec.pricing.as_ref(), &metrics.usage);
    for w in &pricing_outcome.warnings {
        sink.warning(w);
    }

    // Sync error/warning counters from the sink into the metrics document.
    // Must happen *after* every sink.warning/error above.
    metrics.errors_count = sink.error_count();
    metrics.warnings_count = sink.warning_count();

    let doc = build_document(
        status,
        &run_id,
        &identity,
        &metrics,
        wall,
        pricing_outcome.block,
        crate::sink::errors_log_rel_path(),
        finished_at.clone(),
    );

    // ---- events first, then the terminal statistics document -------------
    // Ordering invariant: events.jsonl/errors.log are flushed before
    // statistics.json lands, so the counts in the document are consistent
    // with the logs on disk.
    let persistence_error =
        persist_terminal_artifacts(&run_dir, sink, &doc, &finished_at, wall, status);

    tracing::info!(
        status = status.as_str(),
        run_dir = %run_dir.display(),
        "run finished"
    );

    Ok(RunOutcome {
        stats: doc,
        run_dir,
        workspace_dir,
        final_text,
        persistence_error,
    })
}

/// Emit `run_finished`, flush the sink, then write `statistics.json`
/// atomically. Returns a persistence report when anything could not land
/// (the run outcome is still returned to the caller).
fn persist_terminal_artifacts(
    run_dir: &std::path::Path,
    sink: EventSink,
    doc: &lmhub_core::StatisticsDocument,
    finished_at: &str,
    wall: Duration,
    status: RunStatus,
) -> Option<String> {
    sink.emit(&RunEvent::RunFinished {
        ts: finished_at.to_string(),
        status: status.as_str().to_string(),
        duration_ms: wall.as_millis() as u64,
    });
    let finalize_result = sink.finalize();

    let stats_path = run_dir.join("statistics.json");
    let write_result = serde_json::to_string_pretty(doc)
        .map_err(|e| std::io::Error::other(format!("serialize statistics: {e}")))
        .and_then(|pretty| {
            lmhub_core::util::write_atomic(&stats_path, format!("{pretty}\n").as_bytes())
        });

    collect_persistence_error(run_dir, finalize_result, write_result)
}

/// Fold terminal persistence failures into one report (`None` = all clean).
///
/// The sink is already finalized, so the marker cannot go through it: append
/// to `errors.log` best-effort and keep the outcome instead of turning a
/// fully-executed run into an opaque error.
fn collect_persistence_error(
    run_dir: &std::path::Path,
    finalize_result: std::io::Result<()>,
    write_result: std::io::Result<()>,
) -> Option<String> {
    use std::io::Write as _;

    let mut parts = Vec::new();
    if let Err(e) = finalize_result {
        parts.push(format!("event persistence failed: {e}"));
    }
    if let Err(e) = write_result {
        parts.push(format!("statistics.json write failed: {e}"));
    }
    if parts.is_empty() {
        return None;
    }
    let message = parts.join("; ");
    tracing::error!(error = %message, run_dir = %run_dir.display(), "run artifacts incomplete");
    let line = format!("{}\tpersistence\t{message}\n", now_ts());
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::sink::errors_log_abs_path(run_dir))
        .and_then(|mut f| f.write_all(line.as_bytes()));
    Some(message)
}

async fn run_loop(
    spec: &RunSpec,
    sink: &EventSink,
    metrics: &mut RunMetrics,
    tool_rt: &ToolRuntime,
    reasoning: ReasoningLevel,
) -> Result<LoopExit, CoreError> {
    let mut messages: Vec<ChatMessage> = vec![ChatMessage::user(spec.task.clone())];
    let mut tools = tool_specs();
    // Models that declare no tool support get a single plain-text turn
    // instead of a doomed request (mirrors the adapter-level degradation).
    if !spec.model.capabilities.tool_call && !tools.is_empty() {
        tools.clear();
        sink.warning(&format!(
            "model {} declares no tool support; running without tools (single turn)",
            spec.model.id
        ));
    }

    // Context budget: models.dev window (when known) minus the output
    // reserve; the operator may cap history further in config.
    let budget = spec
        .model
        .context_window
        .map(|window| crate::budget::ContextBudget {
            context_window: window,
            max_history_tokens: spec.max_history_tokens,
            tool_result_max_bytes: spec.tool_result_max_bytes,
        });

    // The static prompt cannot know the configured command allowlist; append
    // the effective list (and path rules) so the model never guesses what
    // the sandbox accepts.
    let system_prompt =
        lmhub_core::augment_system_prompt(&spec.system_prompt, &spec.sandbox.allowed_commands);

    for turn in 1..=spec.max_turns.max(1) {
        if spec.cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        sink.emit(&RunEvent::TurnStarted { ts: now_ts(), turn });

        if let Some(budget) = &budget {
            let warnings = budget.enforce(
                &system_prompt,
                &tools,
                &mut messages,
                spec.max_output_tokens as u64,
            )?;
            for warning in warnings {
                sink.warning(&warning);
            }
        }

        let request = ChatRequest::new(spec.model.id.clone(), system_prompt.clone())
            .with_tools(tools.clone())
            .with_messages(messages.clone())
            .with_reasoning(reasoning)
            .with_max_tokens(spec.max_output_tokens)
            .with_prompt_cache(spec.enable_prompt_cache);

        // Provider failures propagate; the caller logs them exactly once.
        // Streaming path: deltas go to the TUI only; the assembled response
        // drives everything else exactly like the non-streaming path.
        use futures::StreamExt;
        use lmhub_core::ChatStreamItem;
        let mut stream = spec.provider.chat_stream(&request).await?;
        let mut streamed_text = String::new();
        let mut response: Option<lmhub_core::ChatResponse> = None;
        while let Some(item) = stream.next().await {
            match item? {
                ChatStreamItem::Delta(lmhub_core::ChatDelta::Text(text)) => {
                    streamed_text.push_str(&text);
                    sink.emit_ui_only(&RunEvent::LlmDelta {
                        ts: now_ts(),
                        turn,
                        text,
                    });
                }
                ChatStreamItem::Delta(lmhub_core::ChatDelta::Thinking(_)) => {}
                ChatStreamItem::Completed(resp) => response = Some(resp),
            }
        }
        let response = response
            .ok_or_else(|| CoreError::Parse("stream ended without a completed response".into()))?;

        // Deltas are UI-only; the assembled response is the source of truth.
        // A mismatch means an adapter dropped or duplicated text — warn, but
        // keep the assembled response. Only checked when the provider
        // actually streamed text (non-SSE protocols deliver `Completed` alone).
        if !streamed_text.is_empty() && streamed_text != response.text {
            sink.warning(&format!(
                "streamed deltas diverged from the assembled response ({} vs {} chars); using the assembled response",
                streamed_text.chars().count(),
                response.text.chars().count()
            ));
        }

        for warning in &response.warnings {
            sink.warning(warning);
        }

        // A turn cut off by the output cap is not a normal completion: warn
        // and carry an explicit truncation note into history.
        if response.stop_reason == lmhub_core::StopReason::Length {
            sink.warning(&format!(
                "turn {turn}: response stopped at max_tokens ({}); the answer may be truncated",
                spec.max_output_tokens
            ));
        }

        let used_cache = response.usage.cache_read_tokens.unwrap_or(0) > 0
            || response.usage.cache_write_tokens.unwrap_or(0) > 0;
        metrics.record_usage(&response.usage);
        metrics.record_llm_duration(std::time::Duration::from_millis(response.duration_ms));
        if !used_cache {
            metrics.note_request_without_cache();
        }

        sink.emit(&RunEvent::LlmResponse {
            ts: now_ts(),
            turn,
            duration_ms: response.duration_ms,
            usage_delta: response.usage,
            stop_reason: response.stop_reason.as_str().to_string(),
        });

        // Assistant turn goes into history *before* tool execution; raw
        // blocks (thinking signatures etc.) round-trip via provider_state.
        let mut assistant_text = response.text.clone();
        if response.stop_reason == lmhub_core::StopReason::Length && !assistant_text.is_empty() {
            assistant_text.push_str("\n[response truncated at max_tokens]");
        }
        messages.push(ChatMessage::assistant_with_tool_calls(
            assistant_text,
            response.tool_calls.clone(),
            response.raw_assistant_message.clone(),
        ));

        if response.tool_calls.is_empty() {
            return Ok(LoopExit::Completed(response.text));
        }

        for call in response.tool_calls {
            let outcome = tool_rt.execute(&call.name, &call.arguments).await;

            if outcome.sandbox_violation {
                if let Some(err) = &outcome.error {
                    sink.violation(err);
                }
            }
            metrics.record_tool_outcome(outcome.success);
            let mut metadata = outcome.metadata.clone();
            if let Some(obj) = metadata.as_object_mut() {
                obj.insert(
                    "arguments_keys".to_string(),
                    json!(argument_keys(&call.arguments)),
                );
            } else {
                metadata = json!({"arguments_keys": argument_keys(&call.arguments)});
            }
            sink.emit(&RunEvent::ToolCall {
                ts: now_ts(),
                turn,
                name: call.name.clone(),
                status: if outcome.success { "success" } else { "failed" }.to_string(),
                duration_ms: outcome.duration_ms,
                metadata,
                error: outcome.error.clone(),
            });

            messages.push(ChatMessage::named_tool_result(
                &call.id,
                &call.name,
                outcome.summary,
                !outcome.success,
            ));
        }
    }

    Ok(LoopExit::TurnLimitReached)
}

fn argument_keys(arguments: &serde_json::Value) -> Vec<String> {
    arguments
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmhub_core::stats::{build_document, RunIdentity};

    fn document() -> lmhub_core::StatisticsDocument {
        let metrics = RunMetrics::new(true, true);
        build_document(
            RunStatus::Completed,
            "run-test",
            &RunIdentity {
                provider: "mock".into(),
                provider_type: "mock".into(),
                family: "Fam".into(),
                model: "m".into(),
                reasoning: "off".into(),
                started_at: "2026-01-01T00:00:00.000Z".into(),
            },
            &metrics,
            Duration::from_millis(5),
            lmhub_core::stats::PricingBlock::unavailable(),
            "errors.log",
            "2026-01-01T00:00:01.000Z".into(),
        )
    }

    #[test]
    fn clean_persistence_writes_events_then_statistics() {
        let dir = tempfile::tempdir().unwrap();
        let sink = EventSink::create(dir.path(), None).unwrap();
        let error = persist_terminal_artifacts(
            dir.path(),
            sink,
            &document(),
            "2026-01-01T00:00:01.000Z",
            Duration::from_millis(5),
            RunStatus::Completed,
        );
        assert!(error.is_none(), "{error:?}");
        assert!(dir.path().join("statistics.json").is_file());
        let events = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        assert!(events.contains("run_finished"), "{events}");
    }

    /// Disk-full simulation via `/dev/full` (every write returns ENOSPC):
    /// the events file cannot flush, `statistics.json` still lands, and the
    /// failure is reported instead of a silent success.
    #[cfg(target_os = "linux")]
    #[test]
    fn disk_full_keeps_statistics_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/dev/full", dir.path().join("events.jsonl")).unwrap();
        let sink = EventSink::create(dir.path(), None).unwrap();
        sink.warning("pre-finalize warning");
        let error = persist_terminal_artifacts(
            dir.path(),
            sink,
            &document(),
            "2026-01-01T00:00:01.000Z",
            Duration::from_millis(5),
            RunStatus::Completed,
        )
        .expect("disk-full must be reported");
        assert!(error.contains("event persistence"), "{error}");
        assert!(
            dir.path().join("statistics.json").is_file(),
            "statistics.json must still be attempted"
        );
        let log = std::fs::read_to_string(dir.path().join("errors.log")).unwrap();
        assert!(log.contains("persistence"), "{log}");
    }
}
