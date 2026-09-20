//! Context-window budgeting for the agent loop.
//!
//! The loop appends assistant turns and tool results forever; without a
//! budget a long run eventually sends a request larger than the model's
//! context window and the provider rejects it (or silently truncates).
//!
//! Rules (deterministic, cheapest first):
//! 1. estimate tokens as `chars / 4` (heuristic — never used for billing);
//! 2. reserve the output budget so the reply always has room;
//! 3. trim the *oldest* oversized tool results to `tool_result_max_bytes`;
//! 4. drop the oldest turn (assistant + its tool results) wholesale;
//! 5. if the task message itself no longer fits, fail fast with
//!    [`CoreError::LimitExceeded`] instead of sending a doomed request.
//!
//! Every trim/drop is reported so the run records a `warning` event naming
//! what was dropped.

use lmhub_core::{ChatMessage, CoreError, Role, ToolSpec};

/// Per-message framing overhead (role tags, separators) in tokens.
const MESSAGE_OVERHEAD_TOKENS: u64 = 4;
/// Slack for the truncation note appended after a trim; messages only within
/// this slack of the cap are left alone (prevents re-trim loops).
const TRIM_NOTE_SLACK: usize = 256;
/// Lower bounds shared with `AppConfig::sanitize`.
pub use lmhub_core::config::{MIN_HISTORY_TOKENS, MIN_TOOL_RESULT_BYTES};

/// Estimate tokens for a piece of text: `ceil(chars / 4)`.
///
/// The 4-chars-per-token heuristic is the standard cheap approximation; it
/// only drives budgeting decisions, so being off by a bit is harmless.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// Estimated tokens a whole request occupies: system + tools + messages +
/// reserved output. Exposed so callers/tests can assert the invariant.
pub fn estimate_request_tokens(
    system_prompt: &str,
    tools: &[ToolSpec],
    messages: &[ChatMessage],
    max_output_tokens: u64,
) -> u64 {
    estimate_tokens(system_prompt)
        .saturating_add(tools_tokens(tools))
        .saturating_add(messages_tokens(messages))
        .saturating_add(MESSAGE_OVERHEAD_TOKENS.saturating_mul(messages.len() as u64))
        .saturating_add(max_output_tokens)
}

fn tools_tokens(tools: &[ToolSpec]) -> u64 {
    tools
        .iter()
        .map(|t| {
            estimate_tokens(&t.name)
                .saturating_add(estimate_tokens(&t.description))
                .saturating_add(estimate_tokens(&t.parameters.to_string()))
        })
        .sum()
}

fn messages_tokens(messages: &[ChatMessage]) -> u64 {
    messages
        .iter()
        .map(|m| {
            let calls = if m.tool_calls.is_empty() {
                String::new()
            } else {
                serde_json::to_string(&m.tool_calls).unwrap_or_default()
            };
            estimate_tokens(&m.content).saturating_add(estimate_tokens(&calls))
        })
        .sum()
}

/// Budget for one run's requests.
#[derive(Debug, Clone, Copy)]
pub struct ContextBudget {
    /// Model context window in tokens (system + tools + messages + output).
    pub context_window: u64,
    /// Optional operator cap on history tokens only (`max_history_tokens`).
    pub max_history_tokens: Option<u64>,
    /// Tool results larger than this are trimmed (oldest first).
    pub tool_result_max_bytes: u64,
}

impl ContextBudget {
    /// Keep `messages` within budget for a request with `max_output_tokens`.
    ///
    /// Mutates `messages` in place; returns warning texts describing what was
    /// trimmed/dropped (`Ok`) or [`CoreError::LimitExceeded`] when even the
    /// task message cannot fit.
    pub fn enforce(
        &self,
        system_prompt: &str,
        tools: &[ToolSpec],
        messages: &mut Vec<ChatMessage>,
        max_output_tokens: u64,
    ) -> Result<Vec<String>, CoreError> {
        let mut warnings = Vec::new();
        // A reply must always have room, but never reserve more than half the
        // window (some models declare windows smaller than our default
        // max_output_tokens).
        let reserve = max_output_tokens.min(self.context_window / 2).max(1);
        let fixed = estimate_tokens(system_prompt)
            .saturating_add(tools_tokens(tools))
            .saturating_add(reserve);

        loop {
            let history = messages_tokens(messages)
                .saturating_add(MESSAGE_OVERHEAD_TOKENS.saturating_mul(messages.len() as u64));
            let over_window = fixed.saturating_add(history) > self.context_window;
            let over_cap = self
                .max_history_tokens
                .is_some_and(|cap| history > cap.max(MIN_HISTORY_TOKENS));
            if !over_window && !over_cap {
                return Ok(warnings);
            }

            // 1) Trim the oldest oversized tool result.
            if let Some(note) = self.trim_oldest_tool_result(messages) {
                warnings.push(note);
                continue;
            }
            // 2) Drop the oldest assistant turn with its tool results.
            if let Some(note) = drop_oldest_turn(messages) {
                warnings.push(note);
                continue;
            }
            // 3) Nothing left to drop: fail before sending.
            let estimated = fixed.saturating_add(history);
            return Err(CoreError::LimitExceeded(format!(
                "context window exhausted: ~{estimated} tokens needed (system+tools+output \
                 reserve {fixed}, history {history}) but the model's context window is {}{}; \
                 shorten the task, reduce history, or pick a larger-context model",
                self.context_window,
                match self.max_history_tokens {
                    Some(cap) => format!(" (max_history_tokens={cap})"),
                    None => String::new(),
                }
            )));
        }
    }

    /// Trim the first (oldest) tool-result message over the byte cap.
    ///
    /// Only messages above `cap + TRIM_NOTE_SLACK` are touched: the
    /// truncation note itself pushes the content slightly over `cap`, and
    /// re-trimming the same message would loop forever.
    fn trim_oldest_tool_result(&self, messages: &mut [ChatMessage]) -> Option<String> {
        let cap = self.tool_result_max_bytes.max(MIN_TOOL_RESULT_BYTES) as usize;
        for msg in messages.iter_mut() {
            if msg.role == Role::Tool && msg.content.len() > cap + TRIM_NOTE_SLACK {
                let before = msg.content.len();
                msg.content = format!(
                    "{}\n[tool result truncated by the context budget: {before} -> {cap} bytes; \
                     re-read or narrow the query if this is needed]",
                    lmhub_core::util::truncate_bytes(&msg.content, cap)
                );
                return Some(format!(
                    "truncated an old tool result ({before} -> {cap} bytes) to fit the context window"
                ));
            }
        }
        None
    }
}

/// Drop the oldest turn group (everything from message index 1 up to the next
/// assistant message). The first message is always the task and is kept.
fn drop_oldest_turn(messages: &mut Vec<ChatMessage>) -> Option<String> {
    if messages.len() <= 2 {
        return None;
    }
    let end = messages
        .iter()
        .enumerate()
        .skip(2)
        .find(|(_, m)| m.role == Role::Assistant)
        .map(|(i, _)| i)
        .unwrap_or(messages.len());
    if end <= 1 {
        return None;
    }
    let dropped = end - 1;
    messages.drain(1..end);
    Some(format!(
        "dropped {dropped} oldest history message(s) to fit the context window"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmhub_core::ToolCallRequest;
    use serde_json::json;

    fn budget(window: u64) -> ContextBudget {
        ContextBudget {
            context_window: window,
            max_history_tokens: None,
            tool_result_max_bytes: 1024,
        }
    }

    fn tool_call_msg(n: usize, result_bytes: usize) -> Vec<ChatMessage> {
        let id = format!("call_{n}");
        vec![
            ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCallRequest {
                    id: id.clone(),
                    name: "run_command".into(),
                    arguments: json!({"argv": ["node", "-e", "1"]}),
                }],
                None,
            ),
            ChatMessage::named_tool_result(&id, "run_command", "x".repeat(result_bytes), false),
        ]
    }

    fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "run_command".into(),
            description: "run".into(),
            parameters: json!({"type":"object"}),
        }]
    }

    #[test]
    fn trims_old_tool_results_before_dropping_history() {
        let mut messages = vec![ChatMessage::user("task")];
        for n in 0..3 {
            messages.extend(tool_call_msg(n, 20_000));
        }
        let warnings = budget(9_000)
            .enforce("sys", &tools(), &mut messages, 256)
            .unwrap();
        assert!(!warnings.is_empty());
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("truncated an old tool result")),
            "{warnings:?}"
        );
        // Task message survives.
        assert_eq!(messages[0].content, "task");
        let total = estimate_request_tokens("sys", &tools(), &messages, 256);
        assert!(total <= 9_000, "estimated {total} over budget");
    }

    #[test]
    fn drops_oldest_turns_when_truncation_is_not_enough() {
        let mut messages = vec![ChatMessage::user("task")];
        for n in 0..12 {
            messages.extend(tool_call_msg(n, 3_000));
        }
        let before = messages.len();
        let warnings = budget(2_000)
            .enforce("sys", &tools(), &mut messages, 256)
            .unwrap();
        assert!(messages.len() < before);
        assert!(
            warnings.iter().any(|w| w.contains("dropped")),
            "{warnings:?}"
        );
        assert_eq!(messages[0].content, "task");
        let total = estimate_request_tokens("sys", &tools(), &messages, 256);
        assert!(total <= 2_000, "estimated {total} over budget");
    }

    #[test]
    fn unfittable_task_fails_with_limit_exceeded() {
        let mut messages = vec![ChatMessage::user("x".repeat(100_000))];
        let err = budget(1_000)
            .enforce("sys", &tools(), &mut messages, 256)
            .unwrap_err();
        assert!(matches!(err, CoreError::LimitExceeded(_)));
        assert_eq!(err.kind(), "limit_exceeded");
        assert!(err.to_string().contains("context window exhausted"));
    }

    #[test]
    fn history_cap_is_enforced_even_when_window_is_large() {
        let mut messages = vec![ChatMessage::user("task")];
        for n in 0..6 {
            messages.extend(tool_call_msg(n, 4_000));
        }
        let cap = ContextBudget {
            context_window: 1_000_000,
            max_history_tokens: Some(500),
            tool_result_max_bytes: 1024,
        };
        let warnings = cap.enforce("sys", &tools(), &mut messages, 256).unwrap();
        assert!(!warnings.is_empty());
        assert!(messages.len() < 13);
    }
}
