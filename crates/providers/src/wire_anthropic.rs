//! Anthropic Messages wire format. Pure functions — easy to test without
//! network.
//!
//! Notable semantics:
//! - prompt caching: explicit `cache_control: {type:"ephemeral"}` on the
//!   system block and the last tool definition;
//! - thinking blocks must be echoed back **with their signature**, so the
//!   parser stores raw content blocks as `raw_assistant_message` which is
//!   round-tripped via `ChatMessage::provider_state`;
//! - usage: `input_tokens` excludes cached input; we normalize to the
//!   runner convention (`input` includes cache reads) so cost/ratio math
//!   stays provider-agnostic.

use lmhub_core::{
    ChatDelta, ChatMessage, ChatRequest, ChatResponse, CoreError, ReasoningLevel, Result, Role,
    StopReason, ToolCallRequest, Usage,
};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy)]
pub struct AnthropicWireOpts {
    /// Send `thinking` config for non-off reasoning levels.
    pub supports_thinking: bool,
}

/// Anthropic thinking budget per effort level.
pub fn thinking_budget(level: ReasoningLevel) -> Option<u32> {
    match level {
        ReasoningLevel::Off => None,
        ReasoningLevel::Low => Some(2_048),
        ReasoningLevel::Medium => Some(8_192),
        ReasoningLevel::High => Some(16_384),
    }
}

pub fn build_chat_payload(req: &ChatRequest, opts: AnthropicWireOpts) -> Value {
    let budget = thinking_budget(req.reasoning);
    let thinking_enabled = opts.supports_thinking && budget.is_some();

    // Thinking requires max_tokens > budget_tokens.
    let max_tokens = match budget {
        Some(b) if thinking_enabled => req.max_tokens.max(b + 1_024),
        _ => req.max_tokens,
    };

    let mut payload = json!({
        "model": req.model,
        "max_tokens": max_tokens,
        "messages": messages_to_wire(&req.messages),
    });

    if !req.system.trim().is_empty() {
        let mut sys_block = json!({"type": "text", "text": req.system});
        if req.enable_prompt_cache {
            sys_block["cache_control"] = json!({"type": "ephemeral"});
        }
        payload["system"] = json!([sys_block]);
    }

    if thinking_enabled {
        payload["thinking"] = json!({"type": "enabled", "budget_tokens": budget.unwrap()});
        // temperature must be unset/1 when thinking; we never set it anyway.
    }

    if !req.tools.is_empty() {
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        // Cache tools+system prefix together via the last tool block.
        if req.enable_prompt_cache {
            if let Some(last) = tools.last_mut() {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
        }
        payload["tools"] = Value::Array(tools);
    }

    payload
}

fn messages_to_wire(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg.role {
            Role::System => { /* handled via top-level system */ }
            Role::User => {
                out.push(json!({
                    "role": "user",
                    "content": [{"type": "text", "text": msg.content}],
                }));
            }
            Role::Assistant => {
                out.push(assistant_to_wire(msg));
            }
            Role::Tool => {
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": msg.tool_call_id.clone().unwrap_or_default(),
                        "content": msg.content,
                        "is_error": msg.is_error,
                    }],
                }));
            }
        }
    }
    out
}

fn assistant_to_wire(msg: &ChatMessage) -> Value {
    // Preferred path: echo the exact raw blocks (preserves thinking signatures).
    if let Some(raw_blocks) = msg.provider_state.as_ref().and_then(|v| v.as_array()) {
        return json!({"role": "assistant", "content": raw_blocks});
    }
    let mut blocks = Vec::new();
    if !msg.content.is_empty() {
        blocks.push(json!({"type": "text", "text": msg.content}));
    }
    for call in &msg.tool_calls {
        blocks.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": call.arguments,
        }));
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": "(empty assistant turn)"}));
    }
    json!({"role": "assistant", "content": blocks})
}

pub fn parse_chat_response(
    body: &Value,
    duration_ms: u64,
    warnings: Vec<String>,
) -> Result<ChatResponse> {
    let content = body
        .get("content")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CoreError::Parse("response has no content[]".into()))?;

    let mut text_parts = Vec::new();
    let mut thinking_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("thinking") => {
                if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                    thinking_parts.push(t.to_string());
                }
            }
            Some("tool_use") => {
                tool_calls.push(ToolCallRequest {
                    id: block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                });
            }
            Some("redacted_thinking") | None | Some(_) => {}
        }
    }

    let stop_reason = match body.get("stop_reason").and_then(|v| v.as_str()) {
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::Length,
        Some("refusal") => StopReason::Refusal,
        None | Some(_) => StopReason::Other,
    };

    let usage_json = body.get("usage").cloned().unwrap_or(Value::Null);
    let usage = parse_usage(&usage_json);

    Ok(ChatResponse {
        text: text_parts.join("\n"),
        thinking: (!thinking_parts.is_empty()).then(|| thinking_parts.join("\n")),
        tool_calls,
        usage,
        stop_reason,
        // Raw blocks (with signatures) go back into history untouched.
        raw_assistant_message: Some(Value::Array(content.clone())),
        warnings,
        duration_ms,
    })
}

/// Anthropic reports cache tokens separately from `input_tokens`; normalize
/// so `input` includes cache reads/writes (runner-wide convention).
fn parse_usage(u: &Value) -> Usage {
    let base_input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_read = u.get("cache_read_input_tokens").and_then(|v| v.as_u64());
    let cache_write = u
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64());

    // Anthropic folds thinking tokens into output_tokens; they are NOT
    // reported separately here — reasoning stays None (no fabrication).
    Usage {
        input_tokens: base_input
            .saturating_add(cache_read.unwrap_or(0))
            .saturating_add(cache_write.unwrap_or(0)),
        output_tokens: output,
        reasoning_tokens: None,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    }
}

/// Parse `{ "data": [ {"id": ...}, ... ] }`.
pub fn parse_models_list(body: &Value) -> Result<Vec<String>> {
    let data = body
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CoreError::Parse("models response has no data[]".into()))?;
    let mut ids: Vec<String> = data
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire_test_util::sample_request;

    #[test]
    fn caching_marks_system_and_last_tool() {
        let mut req = sample_request(ReasoningLevel::Off);
        req.messages
            .push(ChatMessage::tool_result("call_1", "file body", false));
        let p = build_chat_payload(
            &req,
            AnthropicWireOpts {
                supports_thinking: false,
            },
        );
        assert_eq!(p["system"][0]["cache_control"]["type"], json!("ephemeral"));
        assert_eq!(p["tools"][0]["cache_control"]["type"], json!("ephemeral"));
        assert!(p.get("thinking").is_none());
        // Tool results are user-side tool_result blocks.
        let msgs = p["messages"].as_array().unwrap();
        assert_eq!(
            msgs.last().unwrap()["content"][0]["type"],
            json!("tool_result")
        );
        assert_eq!(
            msgs.last().unwrap()["content"][0]["tool_use_id"],
            json!("call_1")
        );
    }

    #[test]
    fn thinking_sets_budget_and_bumps_max_tokens() {
        let mut req = sample_request(ReasoningLevel::High);
        req.max_tokens = 1024;
        let p = build_chat_payload(
            &req,
            AnthropicWireOpts {
                supports_thinking: true,
            },
        );
        assert_eq!(p["thinking"]["budget_tokens"], json!(16_384));
        assert_eq!(p["max_tokens"], json!(17_408));
    }

    #[test]
    fn parses_blocks_usage_and_preserves_raw() {
        let raw_content = json!([
            {"type":"thinking","thinking":"hmm","signature":"sig123"},
            {"type":"text","text":"Using a tool."},
            {"type":"tool_use","id":"tu_1","name":"read_file","input":{"path":"x"}}
        ]);
        let body = json!({
            "content": raw_content,
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 50,
                      "cache_read_input_tokens": 20, "cache_creation_input_tokens": 5}
        });
        let resp = parse_chat_response(&body, 10, Vec::new()).unwrap();
        assert_eq!(resp.text, "Using a tool.");
        assert_eq!(resp.thinking.as_deref(), Some("hmm"));
        assert_eq!(resp.tool_calls[0].id, "tu_1");
        // normalized: 100 + 20 + 5
        assert_eq!(resp.usage.input_tokens, 125);
        assert_eq!(resp.usage.cache_read_tokens, Some(20));
        assert_eq!(resp.usage.cache_write_tokens, Some(5));
        assert_eq!(resp.usage.reasoning_tokens, None, "no fabricated metrics");
        assert_eq!(
            resp.raw_assistant_message.as_ref().unwrap()[0]["signature"],
            json!("sig123")
        );
    }

    #[test]
    fn round_trips_raw_blocks_into_next_request() {
        let raw = serde_json::json!([
            {"type":"thinking","thinking":"h","signature":"s"},
            {"type":"tool_use","id":"t1","name":"n","input":{}}
        ]);
        let msg = ChatMessage::assistant_with_tool_calls("", vec![], Some(raw));
        let wire = messages_to_wire(&[msg]);
        assert_eq!(wire[0]["content"][0]["signature"], json!("s"));
    }
}

// ---------------------------------------------------------------------------
// Streaming (SSE) support
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;

#[derive(Debug, Default)]
enum BlockAcc {
    #[default]
    Empty,
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
    },
    Tool {
        id: String,
        name: String,
        json: String,
    },
}

/// Accumulates Anthropic SSE events (`message_start` … `message_stop`)
/// into the exact same [`ChatResponse`] the non-streaming parser produces —
/// including raw content blocks with thinking signatures.
#[derive(Debug, Default)]
pub struct AnthropicStreamAccumulator {
    blocks: BTreeMap<i64, BlockAcc>,
    order: Vec<i64>,
    base_input: u64,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    output_tokens: u64,
    stop_reason: Option<StopReason>,
}

impl AnthropicStreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one SSE event (`event` name + parsed `data`). Returns a
    /// UI-facing delta when present.
    pub fn feed(&mut self, event: Option<&str>, data: &Value) -> Option<ChatDelta> {
        match event {
            Some("message_start") => {
                let u = data
                    .pointer("/message/usage")
                    .cloned()
                    .unwrap_or(Value::Null);
                self.cache_read = u.get("cache_read_input_tokens").and_then(|v| v.as_u64());
                self.cache_write = u
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64());
                let base = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                self.base_input = base;
            }
            Some("content_block_start") => {
                let index = data.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                let block = data.get("content_block").cloned().unwrap_or(Value::Null);
                let acc = match block.get("type").and_then(|v| v.as_str()) {
                    Some("text") => BlockAcc::Text(String::new()),
                    Some("thinking") => BlockAcc::Thinking {
                        thinking: String::new(),
                        signature: String::new(),
                    },
                    Some("tool_use") => BlockAcc::Tool {
                        id: block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .into(),
                        name: block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .into(),
                        json: String::new(),
                    },
                    _ => BlockAcc::Empty,
                };
                self.order.push(index);
                self.blocks.insert(index, acc);
            }
            Some("content_block_delta") => {
                let index = data.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                let delta = data.get("delta").cloned().unwrap_or(Value::Null);
                match delta.get("type").and_then(|v| v.as_str()) {
                    Some("text_delta") => {
                        let t = delta
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if let Some(BlockAcc::Text(buf)) = self.blocks.get_mut(&index) {
                            buf.push_str(t);
                        }
                        return (!t.is_empty()).then(|| ChatDelta::Text(t.to_string()));
                    }
                    Some("thinking_delta") => {
                        let t = delta
                            .get("thinking")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if let Some(BlockAcc::Thinking { thinking, .. }) =
                            self.blocks.get_mut(&index)
                        {
                            thinking.push_str(t);
                        }
                        return (!t.is_empty()).then(|| ChatDelta::Thinking(t.to_string()));
                    }
                    Some("signature_delta") => {
                        let sig = delta
                            .get("signature")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if let Some(BlockAcc::Thinking { signature, .. }) =
                            self.blocks.get_mut(&index)
                        {
                            signature.push_str(sig);
                        }
                    }
                    Some("input_json_delta") => {
                        let pj = delta
                            .get("partial_json")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if let Some(BlockAcc::Tool { json, .. }) = self.blocks.get_mut(&index) {
                            json.push_str(pj);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(reason) = data.pointer("/delta/stop_reason").and_then(|v| v.as_str()) {
                    self.stop_reason = Some(match reason {
                        "end_turn" | "stop_sequence" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::Length,
                        "refusal" => StopReason::Refusal,
                        _ => StopReason::Other,
                    });
                }
                if let Some(out) = data
                    .pointer("/usage/output_tokens")
                    .and_then(|v| v.as_u64())
                {
                    self.output_tokens = out;
                }
            }
            _ => {}
        }
        None
    }

    /// Assemble the final response (call on `message_stop` / stream end).
    pub fn finish(self, duration_ms: u64) -> Result<ChatResponse> {
        let mut text_parts = Vec::new();
        let mut thinking_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut raw_blocks = Vec::new();

        for idx in &self.order {
            match self.blocks.get(idx) {
                Some(BlockAcc::Text(t)) => {
                    raw_blocks.push(json!({"type": "text", "text": t}));
                    text_parts.push(t.clone());
                }
                Some(BlockAcc::Thinking {
                    thinking,
                    signature,
                }) => {
                    raw_blocks.push(json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": signature,
                    }));
                    thinking_parts.push(thinking.clone());
                }
                Some(BlockAcc::Tool { id, name, json }) => {
                    let arguments: Value = serde_json::from_str(json).unwrap_or_else(|_| json!({}));
                    raw_blocks.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": arguments,
                    }));
                    tool_calls.push(ToolCallRequest {
                        id: id.clone(),
                        name: name.clone(),
                        arguments,
                    });
                }
                _ => {}
            }
        }

        // Same normalization as non-streaming: runner `input` includes cache.
        let input = self
            .base_input
            .saturating_add(self.cache_read.unwrap_or(0))
            .saturating_add(self.cache_write.unwrap_or(0));

        Ok(ChatResponse {
            text: text_parts.join("\n"),
            thinking: (!thinking_parts.is_empty()).then(|| thinking_parts.join("\n")),
            tool_calls,
            usage: Usage {
                input_tokens: input,
                output_tokens: self.output_tokens,
                reasoning_tokens: None, // folded into output by Anthropic
                cache_read_tokens: self.cache_read,
                cache_write_tokens: self.cache_write,
            },
            stop_reason: self.stop_reason.unwrap_or(StopReason::Other),
            raw_assistant_message: (!raw_blocks.is_empty()).then_some(Value::Array(raw_blocks)),
            warnings: Vec::new(),
            duration_ms,
        })
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn full_thinking_tool_loop_round_trip() {
        let mut acc = AnthropicStreamAccumulator::new();

        let _ = acc.feed(
            Some("message_start"),
            &json!({"message": {"usage": {"input_tokens": 100,
                                          "cache_read_input_tokens": 20,
                                          "cache_creation_input_tokens": 5}}}),
        );
        let _ = acc.feed(
            Some("content_block_start"),
            &json!({"index": 0, "content_block": {"type": "thinking"}}),
        );
        let d = acc.feed(
            Some("content_block_delta"),
            &json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
        );
        assert_eq!(d, Some(ChatDelta::Thinking("hmm".into())));
        let _ = acc.feed(
            Some("content_block_delta"),
            &json!({"index": 0, "delta": {"type": "signature_delta", "signature": "sig1"}}),
        );
        let _ = acc.feed(
            Some("content_block_start"),
            &json!({"index": 1, "content_block": {"type": "text"}}),
        );
        let d = acc.feed(
            Some("content_block_delta"),
            &json!({"index": 1, "delta": {"type": "text_delta", "text": "Answer"}}),
        );
        assert_eq!(d, Some(ChatDelta::Text("Answer".into())));
        let _ = acc.feed(Some("content_block_stop"), &json!({"index": 1}));
        let _ = acc.feed(
            Some("content_block_start"),
            &json!({"index": 2, "content_block": {"type": "tool_use", "id": "tu1", "name": "read_file"}}),
        );
        let _ = acc.feed(
            Some("content_block_delta"),
            &json!({"index": 2, "delta": {"type": "input_json_delta", "partial_json": (r#"{"path": "a"#)}}),
        );
        let _ = acc.feed(
            Some("content_block_delta"),
            &json!({"index": 2, "delta": {"type": "input_json_delta", "partial_json": (r#".txt"}"#)}}),
        );
        let _ = acc.feed(
            Some("message_delta"),
            &json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 42}}),
        );

        let resp = acc.finish(10).unwrap();
        assert_eq!(resp.text, "Answer");
        assert_eq!(resp.thinking.as_deref(), Some("hmm"));
        assert_eq!(resp.tool_calls[0].arguments["path"], json!("a.txt"));
        // normalized: 100 + 20 + 5
        assert_eq!(resp.usage.input_tokens, 125);
        assert_eq!(resp.usage.output_tokens, 42);
        assert_eq!(resp.stop_reason, StopReason::ToolUse);

        // raw blocks must carry the signature for history round-trip
        let raw = resp.raw_assistant_message.unwrap();
        assert_eq!(raw[0]["type"], json!("thinking"));
        assert_eq!(raw[0]["signature"], json!("sig1"));
        assert_eq!(raw[2]["input"]["path"], json!("a.txt"));
    }
}
