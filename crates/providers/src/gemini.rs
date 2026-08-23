//! Google Gemini native adapter (Generative Language API, `generateContent`).
//!
//! Notable mappings:
//! - reasoning effort → `generationConfig.thinkingConfig.thinkingBudget`;
//! - `usageMetadata.thoughtsTokenCount` is reported as **reasoning** tokens;
//!   whether `candidatesTokenCount` also includes them differs between the
//!   native API and Vertex AI — `parse_usage` normalizes via `totalTokenCount`
//!   so output is never double-counted;
//! - `cachedContentTokenCount` → cache read tokens;
//! - tool calls: `functionCall` parts; results: `functionResponse` parts.

use crate::http;
use lmhub_core::{
    ChatDelta, ChatMessage, ChatRequest, ChatResponse, CoreError, ReasoningLevel, Result, Role,
    StopReason, ToolCallRequest, Usage,
};
use serde_json::{json, Value};
use std::time::Instant;

pub fn thinking_budget(level: ReasoningLevel) -> Option<i64> {
    match level {
        ReasoningLevel::Off => None,
        // Gemini's thinkingBudget floor is 1024; the API's ceiling (24576)
        // is shared by the strongest levels.
        ReasoningLevel::Minimal | ReasoningLevel::Low => Some(1_024),
        ReasoningLevel::Medium => Some(8_192),
        ReasoningLevel::High | ReasoningLevel::XHigh | ReasoningLevel::Max => Some(24_576),
    }
}

pub fn chat_url(base_url: &str, model: &str) -> String {
    format!(
        "{}/models/{}:generateContent",
        base_url.trim_end_matches('/'),
        model
    )
}

pub fn build_payload(request: &ChatRequest) -> Value {
    let mut payload = json!({
        "contents": contents_to_wire(&request.messages),
    });

    if !request.system.trim().is_empty() {
        payload["systemInstruction"] = json!({
            "parts": [{"text": request.system}],
        });
    }

    // maxOutputTokens must exceed the thinking budget when thinking is
    // enabled (mirrors the Anthropic wire layer).
    let mut max_output = request.max_tokens;
    if let Some(budget) = thinking_budget(request.reasoning) {
        if (max_output as i64) <= budget {
            max_output = (budget + 1_024) as u32;
        }
    }
    let mut generation_config = json!({"maxOutputTokens": max_output});
    if let Some(budget) = thinking_budget(request.reasoning) {
        generation_config["thinkingConfig"] = json!({"thinkingBudget": budget});
    }
    payload["generationConfig"] = generation_config;

    if !request.tools.is_empty() {
        let decls: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        payload["tools"] = json!([{"functionDeclarations": decls}]);
    }

    payload
}

fn contents_to_wire(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        let role = match msg.role {
            Role::User | Role::System | Role::Tool => "user",
            Role::Assistant => "model",
        };
        let mut parts: Vec<Value> = Vec::new();
        match msg.role {
            Role::Tool => {
                // Gemini keys functionResponse by FUNCTION NAME (not call id).
                let name = msg
                    .tool_name
                    .clone()
                    .unwrap_or_else(|| msg.tool_call_id.clone().unwrap_or_default());
                parts.push(json!({
                    "functionResponse": {
                        "name": name,
                        "response": {"result": msg.content},
                    },
                }));
            }
            _ => {
                if !msg.content.is_empty() {
                    parts.push(json!({"text": msg.content}));
                }
                for call in &msg.tool_calls {
                    parts.push(json!({
                        "functionCall": {"name": call.name, "args": call.arguments},
                    }));
                }
                if parts.is_empty() {
                    parts.push(json!({"text": "(empty turn)"}));
                }
            }
        }
        out.push(json!({"role": role, "parts": parts}));
    }
    out
}

pub fn parse_response(
    body: &Value,
    duration_ms: u64,
    warnings: Vec<String>,
) -> Result<ChatResponse> {
    let candidate = body
        .get("candidates")
        .and_then(|c| c.get(0))
        .ok_or_else(|| CoreError::Parse("response has no candidates[0]".into()))?;
    let content = candidate.get("content").cloned().unwrap_or(Value::Null);

    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for part in content
        .get("parts")
        .and_then(|p| p.as_array())
        .unwrap_or(&vec![])
    {
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            text_parts.push(text.to_string());
        }
        if let Some(call) = part.get("functionCall") {
            tool_calls.push(ToolCallRequest {
                id: format!(
                    "call-{}",
                    call.get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                ),
                name: call
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                arguments: call.get("args").cloned().unwrap_or_else(|| json!({})),
            });
        }
    }

    let stop_reason = match candidate.get("finishReason").and_then(|v| v.as_str()) {
        Some("STOP") | Some("FINISH_REASON_UNSPECIFIED") => StopReason::EndTurn,
        Some("MAX_TOKENS") => StopReason::Length,
        Some("SAFETY") | Some("RECITATION") => StopReason::Refusal,
        None | Some(_) => StopReason::Other,
    };

    let usage_json = body.get("usageMetadata").cloned().unwrap_or(Value::Null);
    let usage = parse_usage(&usage_json);

    Ok(ChatResponse {
        text: text_parts.join("\n"),
        thinking: None,
        tool_calls,
        usage,
        stop_reason,
        raw_assistant_message: None,
        warnings,
        duration_ms,
    })
}

/// Gemini usage normalization.
///
/// On the native API, `totalTokenCount = prompt + candidates + toolUse +
/// thoughts` — thinking tokens are reported separately from
/// `candidatesTokenCount` and billed as output. On Vertex AI,
/// `candidatesTokenCount` already includes thinking tokens. Mirror
/// LiteLLM's heuristic (`prompt + candidates + toolUse == total` implies
/// inclusive) so output is never double-counted on either path.
fn parse_usage(u: &Value) -> Usage {
    let prompt = u
        .get("promptTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = u
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let thoughts = u.get("thoughtsTokenCount").and_then(|v| v.as_u64());
    let cached = u.get("cachedContentTokenCount").and_then(|v| v.as_u64());
    let tool_use = u
        .get("toolUsePromptTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total = u.get("totalTokenCount").and_then(|v| v.as_u64());
    let thoughts_included = total
        .map(|t| prompt.saturating_add(output).saturating_add(tool_use) >= t)
        .unwrap_or(false);
    let output_tokens = if thoughts_included {
        output
    } else {
        output.saturating_add(thoughts.unwrap_or(0))
    };
    Usage {
        input_tokens: prompt,
        output_tokens,
        reasoning_tokens: thoughts,
        cache_read_tokens: cached,
        cache_write_tokens: None,
    }
}

pub fn parse_models_list(body: &Value) -> Result<Vec<String>> {
    let models = body
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CoreError::Parse("models response has no models[]".into()))?;
    let mut ids = Vec::new();
    for m in models {
        if let Some(name) = m.get("name").and_then(|v| v.as_str()) {
            ids.push(name.trim_start_matches("models/").to_string());
        }
    }
    ids.sort();
    Ok(ids)
}

pub async fn chat(
    http_client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    request: &ChatRequest,
) -> Result<ChatResponse> {
    let url = chat_url(base_url, &request.model);
    let headers = vec![("x-goog-api-key".to_string(), api_key.to_string())];
    let started = Instant::now();
    let body = http::post_json(http_client, &url, headers, &build_payload(request)).await?;
    let duration_ms = started.elapsed().as_millis() as u64;
    parse_response(&body, duration_ms, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire_test_util::sample_request;
    use serde_json::json;

    #[test]
    fn payload_maps_system_tools_and_thinking() {
        let mut req = sample_request(ReasoningLevel::Medium);
        req.max_tokens = 2_048;
        req.messages
            .push(lmhub_core::ChatMessage::named_tool_result(
                "call-1",
                "read_file",
                "file body",
                false,
            ));
        let p = build_payload(&req);
        assert_eq!(
            p["systemInstruction"]["parts"][0]["text"],
            json!("You are a coding agent.")
        );
        // maxOutputTokens is bumped above the thinking budget (8_192) so
        // Gemini does not reject the request.
        assert_eq!(p["generationConfig"]["maxOutputTokens"], json!(9_216));
        assert_eq!(
            p["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(8_192)
        );
        assert_eq!(
            p["tools"][0]["functionDeclarations"][0]["name"],
            json!("read_file")
        );

        // last message is the tool result → user role with functionResponse
        let contents = p["contents"].as_array().unwrap();
        let last = contents.last().unwrap();
        assert_eq!(last["role"], json!("user"));
        assert_eq!(
            last["parts"][0]["functionResponse"]["name"],
            json!("read_file")
        );
    }

    #[test]
    fn parses_function_call_and_usage() {
        let body = json!({
            "candidates": [{"finishReason": "STOP", "content": {"role": "model", "parts": [
                {"text": "calling"},
                {"functionCall": {"name": "read_file", "args": {"path": "x"}}}
            ]}}],
            "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 20,
                              "thoughtsTokenCount": 5, "cachedContentTokenCount": 40}
        });
        let resp = parse_response(&body, 10, Vec::new()).unwrap();
        assert_eq!(resp.text, "calling");
        assert_eq!(resp.tool_calls[0].name, "read_file");
        // No totalTokenCount → assume thoughts are NOT included in
        // candidates (native API): output adds thoughts, reasoning reported.
        assert_eq!(resp.usage.output_tokens, 25);
        assert_eq!(resp.usage.reasoning_tokens, Some(5));
        assert_eq!(resp.usage.cache_read_tokens, Some(40));
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn usage_does_not_double_count_when_candidates_include_thoughts() {
        // Vertex AI style: totalTokenCount == prompt + candidates, meaning
        // candidatesTokenCount already includes the thinking tokens.
        let usage = parse_usage(&json!({
            "promptTokenCount": 100,
            "candidatesTokenCount": 25,
            "thoughtsTokenCount": 5,
            "totalTokenCount": 125
        }));
        assert_eq!(usage.output_tokens, 25);
        assert_eq!(usage.reasoning_tokens, Some(5));
    }

    #[test]
    fn strips_models_prefix_in_listing() {
        let body = json!({"models": [{"name": "models/gemini-2.5-pro"}, {"name": "models/gemini-2.5-flash"}]});
        let ids = parse_models_list(&body).unwrap();
        assert_eq!(ids, vec!["gemini-2.5-flash", "gemini-2.5-pro"]);
    }

    #[test]
    fn url_shape() {
        assert_eq!(
            chat_url("https://generativelanguage.googleapis.com/v1beta", "gemini-2.5-pro"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent"
        );
    }
}

// ---------------------------------------------------------------------------
// Streaming (SSE) support
// ---------------------------------------------------------------------------

/// Accumulates `streamGenerateContent?alt=sse` chunks into a synthesized
/// response body and reuses [`parse_response`] — single source of truth for
/// parsing semantics.
#[derive(Debug, Default)]
pub struct GeminiStreamAccumulator {
    parts: Vec<Value>,
    usage: Option<Value>,
    finish_reason: Option<String>,
}

impl GeminiStreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one parsed SSE data chunk; returns a UI-facing delta when the
    /// chunk carried text.
    pub fn feed(&mut self, chunk: &Value) -> Option<ChatDelta> {
        let candidate = chunk.get("candidates").and_then(|c| c.get(0));
        let mut delta_out = None;
        if let Some(parts) = candidate
            .and_then(|c| c.pointer("/content/parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    self.parts.push(json!({"text": text}));
                    delta_out = Some(ChatDelta::Text(text.to_string()));
                } else if let Some(call) = part.get("functionCall") {
                    self.parts.push(json!({"functionCall": call}));
                }
            }
        }
        if let Some(fr) = candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(|v| v.as_str())
        {
            self.finish_reason = Some(fr.to_string());
        }
        if let Some(u) = chunk.get("usageMetadata").filter(|u| u.is_object()) {
            self.usage = Some(u.clone());
        }
        delta_out
    }

    pub fn finish(self, duration_ms: u64) -> Result<ChatResponse> {
        // Merge adjacent text parts so chunked text is not joined with '\n'
        // by parse_response.
        let mut merged: Vec<Value> = Vec::with_capacity(self.parts.len());
        for part in self.parts {
            let is_text_part = part.get("text").is_some();
            if is_text_part {
                if let Some(Value::String(last)) = merged.last_mut().and_then(|p| p.get_mut("text"))
                {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        last.push_str(text);
                        continue;
                    }
                }
            }
            merged.push(part);
        }
        let mut candidate = json!({"content": {"parts": merged}});
        if let Some(fr) = &self.finish_reason {
            candidate["finishReason"] = json!(fr);
        }
        let mut body = json!({"candidates": [candidate]});
        if let Some(u) = self.usage {
            body["usageMetadata"] = u;
        }
        parse_response(&body, duration_ms, Vec::new())
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accumulates_split_text_function_call_and_usage() {
        let mut acc = GeminiStreamAccumulator::new();
        let d1 = acc.feed(&json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "Hel"}]}}]
        }));
        let _ = acc.feed(&json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "lo"},
                {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}}]}}],
            "usageMetadata": {"promptTokenCount": 50, "candidatesTokenCount": 7,
                              "thoughtsTokenCount": 2, "cachedContentTokenCount": 10}
        }));
        assert_eq!(d1, Some(ChatDelta::Text("Hel".into())));

        let resp = acc.finish(9).unwrap();
        assert_eq!(resp.text, "Hello");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].arguments["path"], json!("a.txt"));
        assert_eq!(resp.usage.output_tokens, 9); // 7 + 2 thoughts
        assert_eq!(resp.usage.reasoning_tokens, Some(2));
        assert_eq!(resp.usage.cache_read_tokens, Some(10));
    }
}

pub fn stream_url(base_url: &str, model: &str) -> String {
    format!(
        "{}/models/{}:streamGenerateContent?alt=sse",
        base_url.trim_end_matches('/'),
        model
    )
}
