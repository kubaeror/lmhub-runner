//! OpenAI Chat Completions wire format (also used by openai-compatible
//! custom providers). Pure functions — easy to test without network.

use lmhub_core::{
    ChatDelta, ChatMessage, ChatRequest, ChatResponse, CoreError, ReasoningLevel, Result, Role,
    StopReason, ToolCallRequest, Usage,
};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiWireOpts {
    /// Send `reasoning_effort` for non-off levels.
    pub include_reasoning_effort: bool,
}

pub fn build_chat_payload(req: &ChatRequest, opts: OpenAiWireOpts) -> Value {
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if !req.system.trim().is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    for msg in &req.messages {
        messages.push(message_to_wire(msg));
    }

    let mut payload = json!({
        "model": req.model,
        "messages": messages,
        "stream": false,
        "max_completion_tokens": req.max_tokens,
    });

    if opts.include_reasoning_effort && req.reasoning != ReasoningLevel::Off {
        payload["reasoning_effort"] = json!(req.reasoning.as_str());
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        payload["tools"] = json!(tools);
        payload["tool_choice"] = json!("auto");
    }
    payload
}

fn message_to_wire(msg: &ChatMessage) -> Value {
    match msg.role {
        Role::System => json!({"role": "system", "content": msg.content}),
        Role::User => json!({"role": "user", "content": msg.content}),
        Role::Assistant => {
            let mut m = json!({"role": "assistant"});
            m["content"] = if msg.content.is_empty() {
                Value::Null
            } else {
                json!(msg.content)
            };
            if !msg.tool_calls.is_empty() {
                let calls: Vec<Value> = msg
                    .tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                "arguments": c.arguments.to_string(),
                            }
                        })
                    })
                    .collect();
                m["tool_calls"] = Value::Array(calls);
            }
            m
        }
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": msg.tool_call_id.clone().unwrap_or_default(),
            "content": msg.content,
        }),
    }
}

pub fn parse_chat_response(
    body: &Value,
    duration_ms: u64,
    mut warnings: Vec<String>,
) -> Result<ChatResponse> {
    let choice = body
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| CoreError::Parse("response has no choices[0]".into()))?;
    let message = choice
        .get("message")
        .ok_or_else(|| CoreError::Parse("choices[0] has no message".into()))?;

    // `content` is a string on OpenAI proper, but some compatible servers
    // return an array of typed parts — concatenate the text ones.
    let text = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };

    // Some OpenAI-compatible servers expose reasoning content here.
    let thinking = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for call in calls {
            let id = call
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let function = call.get("function").cloned().unwrap_or(Value::Null);
            let name = function
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let raw_args = function
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let arguments: Value = serde_json::from_str(raw_args).unwrap_or_else(|_| {
                warnings.push(format!(
                    "tool call {name:?} had non-JSON arguments; coerced to empty object"
                ));
                json!({})
            });
            tool_calls.push(ToolCallRequest {
                id,
                name,
                arguments,
            });
        }
    }

    let stop_reason = match choice.get("finish_reason").and_then(|v| v.as_str()) {
        Some("stop") | Some("end_turn") => StopReason::EndTurn,
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("length") | Some("max_tokens") => StopReason::Length,
        Some("content_filter") => StopReason::Refusal,
        None | Some(_) => StopReason::Other,
    };

    let usage_json = body.get("usage").cloned().unwrap_or(Value::Null);
    let usage = parse_usage(&usage_json);

    Ok(ChatResponse {
        text,
        thinking,
        tool_calls,
        usage,
        stop_reason,
        raw_assistant_message: None,
        warnings,
        duration_ms,
    })
}

/// OpenAI reports `prompt_tokens` *including* cached tokens; cached tokens
/// appear in `prompt_tokens_details.cached_tokens`. We keep that convention:
/// `input` includes cache reads so `cache_hit_ratio = cache_read / input`.
fn parse_usage(u: &Value) -> Usage {
    let prompt = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let completion = u
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let details_in = u.get("prompt_tokens_details");
    let details_out = u.get("completion_tokens_details");
    Usage {
        input_tokens: prompt,
        output_tokens: completion,
        reasoning_tokens: details_out
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64()),
        cache_read_tokens: details_in
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        cache_write_tokens: None,
    }
}

/// Parse `{ "data": [ {"id": ...}, ... ] }` model listings.
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
    use crate::wire_test_util::*;

    #[test]
    fn payload_contains_tools_and_reasoning() {
        let req = sample_request(ReasoningLevel::High);
        let p = build_chat_payload(
            &req,
            OpenAiWireOpts {
                include_reasoning_effort: true,
            },
        );
        assert_eq!(p["model"], json!("test-model"));
        assert_eq!(p["reasoning_effort"], json!("high"));
        assert_eq!(p["max_completion_tokens"], json!(1024));
        assert_eq!(p["messages"].as_array().unwrap().len(), 3); // system + user + assistant(tool_calls)
        assert_eq!(p["messages"][0]["role"], json!("system"));
        assert_eq!(p["messages"][2]["tool_calls"][0]["id"], json!("call_1"));
        assert_eq!(p["tools"][0]["function"]["name"], json!("read_file"));
    }

    #[test]
    fn omits_reasoning_when_disabled() {
        let req = sample_request(ReasoningLevel::Off);
        let p = build_chat_payload(
            &req,
            OpenAiWireOpts {
                include_reasoning_effort: true,
            },
        );
        assert!(p.get("reasoning_effort").is_none());
    }

    #[test]
    fn passes_extended_effort_levels_through() {
        for level in [
            ReasoningLevel::Minimal,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
        ] {
            let req = sample_request(level);
            let p = build_chat_payload(
                &req,
                OpenAiWireOpts {
                    include_reasoning_effort: true,
                },
            );
            assert_eq!(p["reasoning_effort"], json!(level.as_str()));
        }
    }

    #[test]
    fn parses_usage_with_cache_and_reasoning_details() {
        let body = json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "content": null,
                "tool_calls": [{"id":"a","type":"function","function":{"name":"t","arguments":"{\"x\":1}"}}]
            }}],
            "usage": {"prompt_tokens": 4218, "completion_tokens": 8930,
                      "prompt_tokens_details": {"cached_tokens": 1800},
                      "completion_tokens_details": {"reasoning_tokens": 6151}}
        });
        let resp = parse_chat_response(&body, 10, Vec::new()).unwrap();
        assert_eq!(resp.usage.input_tokens, 4218);
        assert_eq!(resp.usage.cache_read_tokens, Some(1800));
        assert_eq!(resp.usage.reasoning_tokens, Some(6151));
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.tool_calls[0].arguments["x"], json!(1));
        // ratio per spec example: 1800/4218
        assert!((1800f64 / 4218.0 - 0.4267).abs() < 0.001);
    }

    #[test]
    fn detects_reasoning_rejections() {
        assert!(is_reasoning_rejection("reasoning_effort is not supported"));
        assert!(is_reasoning_rejection(
            "This model always engages in thinking and cannot be disabled; please use low, high, or max"
        ));
        assert!(is_reasoning_rejection(
            "reasoning is not supported by this model"
        ));
        assert!(!is_reasoning_rejection("stream_options is not supported"));
        assert!(!is_reasoning_rejection("invalid api key"));
        // opencode gateway wording: free-tier models only accept no_think/low/high.
        assert!(is_reasoning_rejection(
            "The request is invalid: reasoning_effort must be one of: no_think, low, high. Please adjust your request."
        ));
    }

    #[test]
    fn parses_suggested_level() {
        assert_eq!(
            suggested_reasoning_level("please use low, high, or max"),
            Some(ReasoningLevel::Low)
        );
        assert_eq!(
            suggested_reasoning_level("set reasoning_effort to xhigh"),
            Some(ReasoningLevel::XHigh)
        );
        assert_eq!(
            suggested_reasoning_level("reasoning_effort unsupported"),
            None
        );
        assert_eq!(suggested_reasoning_level(""), None);
        // opencode wording: earliest named level wins (low before high).
        assert_eq!(
            suggested_reasoning_level("reasoning_effort must be one of: no_think, low, high"),
            Some(ReasoningLevel::Low)
        );
    }
}

// ---------------------------------------------------------------------------
// Streaming (SSE) support
// ---------------------------------------------------------------------------

/// Payload for streaming requests. `stream_options.include_usage` is added
/// because OpenAI only reports usage in the final chunk with it; some
/// compatible servers reject the field, so callers must be ready to retry
/// once without it (see [`strip_stream_options`]).
pub fn build_stream_payload(req: &ChatRequest, opts: OpenAiWireOpts) -> Value {
    let mut payload = build_chat_payload(req, opts);
    payload["stream"] = json!(true);
    payload["stream_options"] = json!({"include_usage": true});
    payload
}

pub fn strip_stream_options(payload: &mut Value) {
    if let Some(obj) = payload.as_object_mut() {
        obj.remove("stream_options");
    }
}

/// Fall back from `max_completion_tokens` to `max_tokens` (many
/// OpenAI-compatible servers and older models only accept the latter).
pub fn use_max_tokens_field(payload: &mut Value) {
    if let Some(obj) = payload.as_object_mut() {
        if let Some(max) = obj.remove("max_completion_tokens") {
            obj.insert("max_tokens".to_string(), max);
        }
    }
}

fn map_finish_reason(raw: &str) -> StopReason {
    match raw {
        "stop" | "end_turn" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" | "max_tokens" => StopReason::Length,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::Other,
    }
}

/// Accumulates streamed OpenAI chat chunks into a final [`ChatResponse`].
#[derive(Debug, Default)]
pub struct OpenAiStreamAccumulator {
    text: String,
    thinking: String,
    tools: std::collections::BTreeMap<u64, ToolAcc>,
    usage: Usage,
    stop_reason: Option<StopReason>,
}

#[derive(Debug, Default)]
struct ToolAcc {
    id: String,
    name: String,
    arguments: String,
}

impl OpenAiStreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one parsed chunk; returns a UI-facing delta when present.
    /// `finish_reason` and `usage` are processed even on chunks that also
    /// carry text deltas (OpenAI sends them together on the final chunk).
    pub fn feed(&mut self, chunk: &Value) -> Option<ChatDelta> {
        let mut delta_out = None;
        let choice = chunk.get("choices").and_then(|c| c.get(0));
        if let Some(delta) = choice.and_then(|c| c.get("delta")) {
            match delta.get("content") {
                Some(Value::String(s)) if !s.is_empty() => {
                    self.text.push_str(s);
                    delta_out = Some(ChatDelta::Text(s.clone()));
                }
                Some(Value::Array(parts)) => {
                    for p in parts {
                        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                            self.text.push_str(t);
                            delta_out = Some(ChatDelta::Text(t.to_string()));
                        }
                    }
                }
                _ => {}
            }
            if delta_out.is_none() {
                if let Some(rc) = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|v| v.as_str())
                {
                    self.thinking.push_str(rc);
                    delta_out = Some(ChatDelta::Thinking(rc.to_string()));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for call in calls {
                    let index = call.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                    let entry = self.tools.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                        if !id.is_empty() {
                            entry.id = id.to_string();
                        }
                    }
                    if let Some(f) = call.get("function") {
                        if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                            if !name.is_empty() {
                                entry.name = name.to_string();
                            }
                        }
                        if let Some(args) = f.get("arguments").and_then(|v| v.as_str()) {
                            entry.arguments.push_str(args);
                        }
                    }
                }
            }
        }
        if let Some(reason) = choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(|v| v.as_str())
        {
            self.stop_reason = Some(map_finish_reason(reason));
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = parse_usage(u);
        }
        delta_out
    }

    /// Assemble the final response (call when the stream ends).
    pub fn finish(mut self, duration_ms: u64) -> Result<ChatResponse> {
        if self.usage.total() == 0 && self.text.is_empty() && self.tools.is_empty() {
            return Err(CoreError::Parse(
                "provider returned an empty stream (no tokens or tool calls) — usually a rate \
                 limit, an unavailable model, or a reasoning level the model rejects; check the \
                 provider's quota and the requested reasoning level"
                    .into(),
            ));
        }
        let mut warnings = Vec::new();
        let tool_calls = self
            .tools
            .into_values()
            .map(|acc| {
                let arguments = match serde_json::from_str::<Value>(&acc.arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        warnings.push(format!(
                            "tool call '{}' had non-JSON arguments ({}); coerced to empty object",
                            acc.name, e
                        ));
                        json!({})
                    }
                };
                ToolCallRequest {
                    id: acc.id,
                    name: acc.name,
                    arguments,
                }
            })
            .collect();
        Ok(ChatResponse {
            text: std::mem::take(&mut self.text),
            thinking: (!self.thinking.is_empty()).then(|| std::mem::take(&mut self.thinking)),
            tool_calls,
            usage: self.usage,
            stop_reason: self.stop_reason.unwrap_or(StopReason::Other),
            raw_assistant_message: None,
            warnings,
            duration_ms,
        })
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use crate::wire_test_util::sample_request;
    use serde_json::json;

    #[test]
    fn stream_payload_flags_usage_options() {
        let req = sample_request(ReasoningLevel::Off);
        let mut p = build_stream_payload(
            &req,
            OpenAiWireOpts {
                include_reasoning_effort: true,
            },
        );
        assert_eq!(p["stream"], json!(true));
        assert_eq!(p["stream_options"]["include_usage"], json!(true));
        strip_stream_options(&mut p);
        assert!(p.get("stream_options").is_none());
        assert_eq!(p["stream"], json!(true));
    }

    #[test]
    fn accumulates_text_tools_and_final_usage() {
        let mut acc = OpenAiStreamAccumulator::new();
        let d1 = acc.feed(&json!({
            "choices": [{"delta": {"role": "assistant", "content": "Hel"}}]}));
        let d2 = acc.feed(&json!({
            "choices": [{"delta": {"content": "lo"}}]}));
        let d3 = acc.feed(&json!({
        "choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "t1", "function": {"name": "read_file",
                "arguments": (r#"{"path": "a"#)}},
            {"index": 0,
                "function": {"arguments": (r#".txt"}"#)}}
        ]}}]}));
        assert_eq!(d1, Some(ChatDelta::Text("Hel".into())));
        assert_eq!(d2, Some(ChatDelta::Text("lo".into())));
        assert!(d3.is_none(), "tool deltas stay internal");

        let resp = acc.finish(10).unwrap();
        assert_eq!(resp.text, "Hello");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "t1");
        assert_eq!(resp.tool_calls[0].arguments["path"], json!("a.txt"));
        assert_eq!(resp.stop_reason, StopReason::Other);
    }

    #[test]
    fn reasoning_and_usage_chunk() {
        let mut acc = OpenAiStreamAccumulator::new();
        assert_eq!(
            acc.feed(&json!({"choices":[{"delta":{"reasoning_content":"think"}}]})),
            Some(ChatDelta::Thinking("think".into()))
        );
        acc.feed(&json!({"choices":[{"delta":{"content":"ans"},"finish_reason":"stop"}]}));
        acc.feed(&json!({
            "choices": [],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4,
                      "prompt_tokens_details": {"cached_tokens": 6},
                      "completion_tokens_details": {"reasoning_tokens": 3}}
        }));
        let resp = acc.finish(5).unwrap();
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.cache_read_tokens, Some(6));
        assert_eq!(resp.usage.reasoning_tokens, Some(3));
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.thinking.as_deref(), Some("think"));
    }

    #[test]
    fn empty_stream_is_parse_error() {
        let err = OpenAiStreamAccumulator::new().finish(1).unwrap_err();
        assert!(matches!(err, CoreError::Parse(_)));
    }
}

/// Broad reasoning-rejection detection: a 4xx explaining that the request's
/// reasoning effort / thinking mode is not acceptable for this model
/// (e.g. "reasoning_effort" rejected, or "always engages in thinking and
/// cannot be disabled; please use low, high, or max"). These must trigger a
/// retry, not a hard failure.
pub fn is_reasoning_rejection(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    if m.contains("reasoning_effort") {
        return true;
    }
    let reasoning_clause = m.contains("reasoning")
        && (m.contains("unsupported")
            || m.contains("not supported")
            || m.contains("invalid")
            || m.contains("disabled")
            || m.contains("available")
            || m.contains("cannot"));
    let thinking_clause = m.contains("thinking")
        && (m.contains("unsupported")
            || m.contains("not supported")
            || m.contains("disabled")
            || m.contains("cannot")
            || m.contains("always"));
    let effort_clause = m.contains("effort")
        && (m.contains("unsupported") || m.contains("invalid") || m.contains("please use"));
    reasoning_clause || thinking_clause || effort_clause
}

/// First concrete reasoning level a rejection message suggests, e.g.
/// "please use low, high, or max" → Low. `None` when the message names no
/// level. The earliest level named in the message wins.
pub fn suggested_reasoning_level(msg: &str) -> Option<ReasoningLevel> {
    let m = msg.to_ascii_lowercase();
    const LADDER: [(&str, ReasoningLevel); 8] = [
        ("xhigh", ReasoningLevel::XHigh),
        ("minimal", ReasoningLevel::Minimal),
        ("medium", ReasoningLevel::Medium),
        ("high", ReasoningLevel::High),
        ("max", ReasoningLevel::Max),
        ("low", ReasoningLevel::Low),
        ("none", ReasoningLevel::Off),
        ("off", ReasoningLevel::Off),
    ];
    LADDER
        .iter()
        .filter_map(|(needle, lvl)| m.find(needle).map(|pos| (pos, *lvl)))
        .min_by_key(|(pos, _)| *pos)
        .map(|(_, lvl)| lvl)
}
