//! Structured run transcript: turns with LLM text and tool calls, folded
//! from the raw `RunEvent` stream. Pure data + logic (no ratatui).

use lmhub_core::{RunEvent, Usage};

/// One tool invocation inside a turn.
#[derive(Debug, Clone)]
pub struct ToolCallEvent {
    pub name: String,
    pub status: String,
    pub duration_ms: u64,
    pub error: Option<String>,
}

/// One agent turn: the model's reply plus the tools it invoked.
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub number: u32,
    /// Streamed assistant text (empty when the model only called tools).
    pub llm_text: String,
    pub tool_calls: Vec<ToolCallEvent>,
    pub usage: Usage,
    pub duration_ms: u64,
    pub stop_reason: String,
}

/// The whole run, folded incrementally from `RunEvent`s.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub turns: Vec<Turn>,
    /// Raw one-line feed of every non-delta event (kept for the raw view).
    pub feed: Vec<String>,
}

const FEED_CAP: usize = 2_000;
const DRAIN: usize = 500;

impl Transcript {
    /// Fold one event into the transcript. Returns true when the event
    /// produced a visible feed line.
    pub fn fold(&mut self, ev: &RunEvent) -> bool {
        match ev {
            RunEvent::TurnStarted { turn, .. } => {
                self.turns.push(Turn {
                    number: *turn,
                    ..Default::default()
                });
            }
            RunEvent::LlmDelta { text, .. } => {
                if let Some(t) = self.turns.last_mut() {
                    t.llm_text.push_str(text);
                }
                return false; // deltas never render as feed lines
            }
            RunEvent::LlmResponse {
                duration_ms,
                usage_delta,
                stop_reason,
                ..
            } => {
                if let Some(t) = self.turns.last_mut() {
                    t.usage = *usage_delta;
                    t.duration_ms = *duration_ms;
                    t.stop_reason = stop_reason.clone();
                }
            }
            RunEvent::ToolCall {
                name,
                status,
                duration_ms,
                error,
                ..
            } => {
                if let Some(t) = self.turns.last_mut() {
                    t.tool_calls.push(ToolCallEvent {
                        name: name.clone(),
                        status: status.clone(),
                        duration_ms: *duration_ms,
                        error: error.clone(),
                    });
                }
            }
            _ => {}
        }
        let line = ev.to_line();
        if !line.is_empty() {
            self.feed.push(line);
            if self.feed.len() > FEED_CAP {
                self.feed.drain(..DRAIN);
            }
        }
        true
    }

    /// Latest turn still streaming (deltas land here).
    pub fn live_turn(&self) -> Option<&Turn> {
        self.turns.last()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty() && self.feed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmhub_core::Usage;

    fn ev_turn(n: u32) -> RunEvent {
        RunEvent::TurnStarted {
            ts: "t".into(),
            turn: n,
        }
    }
    fn ev_delta(n: u32, text: &str) -> RunEvent {
        RunEvent::LlmDelta {
            ts: "t".into(),
            turn: n,
            text: text.into(),
        }
    }

    #[test]
    fn folds_streaming_text_into_turns() {
        let mut tr = Transcript::default();
        tr.fold(&ev_turn(1));
        assert!(!tr.fold(&ev_delta(1, "hello ")));
        tr.fold(&ev_delta(1, "world"));
        assert_eq!(tr.live_turn().unwrap().llm_text, "hello world");
        // Deltas never add feed lines; the turn start did.
        assert_eq!(tr.feed, vec!["— turn 1"]);
    }

    #[test]
    fn tool_calls_attach_to_current_turn() {
        let mut tr = Transcript::default();
        tr.fold(&ev_turn(1));
        tr.fold(&RunEvent::ToolCall {
            ts: "t".into(),
            turn: 1,
            name: "run_command".into(),
            status: "success".into(),
            duration_ms: 12,
            metadata: serde_json::json!({}),
            error: None,
        });
        assert_eq!(tr.turns.len(), 1);
        assert_eq!(tr.turns[0].tool_calls.len(), 1);
        assert_eq!(tr.turns[0].tool_calls[0].name, "run_command");
        // One feed line per non-delta event: turn start + tool call.
        assert_eq!(tr.feed.len(), 2);
    }

    #[test]
    fn llm_response_captures_usage() {
        let mut tr = Transcript::default();
        tr.fold(&ev_turn(1));
        tr.fold(&RunEvent::LlmResponse {
            ts: "t".into(),
            turn: 1,
            duration_ms: 500,
            usage_delta: Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Default::default()
            },
            stop_reason: "end_turn".into(),
        });
        let t = &tr.turns[0];
        assert_eq!(t.usage.input_tokens, 100);
        assert_eq!(t.duration_ms, 500);
        assert_eq!(t.stop_reason, "end_turn");
    }

    #[test]
    fn feed_caps_and_drains() {
        let mut tr = Transcript::default();
        for i in 0..(FEED_CAP + 100) {
            tr.fold(&ev_turn(i as u32));
        }
        assert!(tr.feed.len() <= FEED_CAP);
    }
}
