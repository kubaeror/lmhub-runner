use crate::usage::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One JSONL record in `events.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    RunStarted {
        ts: String,
        provider: String,
        provider_type: String,
        family: String,
        model: String,
        reasoning: String,
        task_chars: u64,
    },
    TurnStarted {
        ts: String,
        turn: u32,
    },
    LlmResponse {
        ts: String,
        turn: u32,
        duration_ms: u64,
        usage_delta: Usage,
        stop_reason: String,
    },
    /// Incremental streamed text — forwarded to the TUI only; intentionally
    /// NOT persisted to events.jsonl (schema stability, no spam).
    #[serde(skip)]
    LlmDelta {
        ts: String,
        turn: u32,
        text: String,
    },
    ToolCall {
        ts: String,
        turn: u32,
        name: String,
        status: String,
        duration_ms: u64,
        /// Safe metadata only (paths inside workspace, argv, exit codes).
        metadata: Value,
        error: Option<String>,
    },
    SandboxViolation {
        ts: String,
        detail: String,
    },
    Warning {
        ts: String,
        message: String,
    },
    Error {
        ts: String,
        kind: String,
        message: String,
    },
    RunFinished {
        ts: String,
        status: String,
        duration_ms: u64,
    },
}

impl RunEvent {
    pub fn ts(&self) -> &str {
        match self {
            Self::RunStarted { ts, .. }
            | Self::TurnStarted { ts, .. }
            | Self::LlmResponse { ts, .. }
            | Self::LlmDelta { ts, .. }
            | Self::ToolCall { ts, .. }
            | Self::SandboxViolation { ts, .. }
            | Self::Warning { ts, .. }
            | Self::Error { ts, .. }
            | Self::RunFinished { ts, .. } => ts,
        }
    }

    /// Compact human-readable line for the TUI feed.
    pub fn to_line(&self) -> String {
        match self {
            Self::RunStarted {
                model, reasoning, ..
            } => {
                format!("▶ run started — {model} (reasoning: {reasoning})")
            }
            Self::TurnStarted { turn, .. } => format!("— turn {turn}"),
            Self::LlmDelta { .. } => String::new(), // rendered via live tail, not the feed
            Self::LlmResponse {
                duration_ms,
                usage_delta,
                stop_reason,
                ..
            } => format!(
                "● llm {} ms | in {} out {}{}{} | stop: {}",
                duration_ms,
                usage_delta.input_tokens,
                usage_delta.output_tokens,
                usage_delta
                    .reasoning_tokens
                    .map(|r| format!(" reason {r}"))
                    .unwrap_or_default(),
                usage_delta
                    .cache_read_tokens
                    .map(|c| format!(" cache-read {c}"))
                    .unwrap_or_default(),
                stop_reason
            ),
            Self::ToolCall {
                turn,
                name,
                status,
                duration_ms,
                error,
                ..
            } => match (status.as_str(), error) {
                ("success", _) => format!("✔ [{t}] {name} ok ({duration_ms} ms)", t = turn),
                _ => format!(
                    "✘ [{}] {} failed ({} ms){}",
                    turn,
                    name,
                    duration_ms,
                    error
                        .as_deref()
                        .map(|e| format!(" — {}", one_line(e)))
                        .unwrap_or_default()
                ),
            },
            Self::SandboxViolation { detail, .. } => {
                format!("⛔ sandbox violation: {}", one_line(detail))
            }
            Self::Warning { message, .. } => format!("⚠ {}", one_line(message)),
            Self::Error { kind, message, .. } => {
                format!("✖ {}: {}", kind, one_line(message))
            }
            Self::RunFinished {
                status,
                duration_ms,
                ..
            } => {
                format!("■ run finished: {} ({} ms)", status, duration_ms)
            }
        }
    }

    /// Reasoning level label helper for RunStarted.
    pub fn reasoning_of(&self) -> Option<&str> {
        match self {
            Self::RunStarted { reasoning, .. } => Some(reasoning.as_str()),
            _ => None,
        }
    }
}

fn one_line(s: &str) -> String {
    let mut out = s.replace(['\n', '\r', '\t'], " ");
    if out.len() > 300 {
        out.truncate(297);
        out.push_str("...");
    }
    out
}
