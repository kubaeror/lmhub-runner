//! Cohere native adapter (`POST {base}/v2/chat`).
//!
//! Differences from OpenAI: content is an array of typed parts, assistant
//! tool calls live under `message.toolCalls`, results use role `"tool"`.

use crate::http;
use lmhub_core::{
    ChatRequest, ChatResponse, CoreError, Result, Role, StopReason, ToolCallRequest, Usage,
};
use serde_json::{json, Value};
use std::time::Instant;

pub fn chat_url(base_url: &str) -> String {
    format!("{}/v2/chat", base_url.trim_end_matches('/'))
}

pub fn build_payload(request: &ChatRequest) -> Value {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if !request.system.trim().is_empty() {
        messages
            .push(json!({"role": "system", "content": [{"type":"text","text": request.system}]}));
    }
    for msg in &request.messages {
        match msg.role {
            Role::System => {
                messages.push(
                    json!({"role": "system", "content": [{"type":"text","text": msg.content}]}),
                );
            }
            Role::User => {
                messages.push(
                    json!({"role": "user", "content": [{"type":"text","text": msg.content}]}),
                );
            }
            Role::Assistant => {
                let mut m = json!({"role": "assistant"});
                if !msg.content.is_empty() {
                    m["content"] = json!([{"type":"text","text": msg.content}]);
                }
                if !msg.tool_calls.is_empty() {
                    let calls: Vec<Value> = msg
                        .tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {"name": c.name, "arguments": c.arguments},
                            })
                        })
                        .collect();
                    // Cohere v2 puts tool calls on the message as `tool_calls`.
                    m["tool_calls"] = Value::Array(calls);
                }
                messages.push(m);
            }
            Role::Tool => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": msg.tool_call_id.clone().unwrap_or_default(),
                    "content": [{"type":"text","text": msg.content}],
                }));
            }
        }
    }

    let mut payload = json!({"model": request.model, "messages": messages});
    payload["max_tokens"] = json!(request.max_tokens);
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.parameters}}))
            .collect();
        payload["tools"] = Value::Array(tools);
    }
    payload
}

pub fn parse_response(
    body: &Value,
    duration_ms: u64,
    warnings: Vec<String>,
) -> Result<ChatResponse> {
    let message = body
        .get("message")
        .ok_or_else(|| CoreError::Parse("cohere response has no message".into()))?;

    let mut text_parts = Vec::new();
    for part in message
        .get("content")
        .and_then(|c| c.as_array())
        .unwrap_or(&vec![])
    {
        if part.get("type").and_then(|v| v.as_str()) == Some("text") {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                text_parts.push(t.to_string());
            }
        }
    }

    let mut tool_calls = Vec::new();
    let calls_arr = message
        .get("tool_calls")
        .or_else(|| message.get("toolCalls"))
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    for call in &calls_arr {
        let function = call.get("function").cloned().unwrap_or(Value::Null);
        tool_calls.push(ToolCallRequest {
            id: call
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .into(),
            name: function
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .into(),
            arguments: function
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
        });
    }

    let stop_reason = match body.get("finish_reason").and_then(|v| v.as_str()) {
        Some("COMPLETE") => StopReason::EndTurn,
        Some("TOOL_CALLS") => StopReason::ToolUse,
        Some("MAX_TOKENS") => StopReason::Length,
        None | Some(_) => StopReason::Other,
    };

    let tokens = body
        .pointer("/usage/tokens")
        .cloned()
        .unwrap_or(Value::Null);
    let usage = Usage {
        input_tokens: tokens
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output_tokens: tokens
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        reasoning_tokens: tokens.get("reasoning_tokens").and_then(|v| v.as_u64()),
        cache_read_tokens: None,
        cache_write_tokens: None,
    };

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

pub async fn chat(
    http_client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    request: &ChatRequest,
) -> Result<ChatResponse> {
    let headers = vec![("authorization".to_string(), format!("Bearer {api_key}"))];
    let started = Instant::now();
    let body = http::post_json(
        http_client,
        &chat_url(base_url),
        headers,
        &build_payload(request),
    )
    .await?;
    parse_response(&body, started.elapsed().as_millis() as u64, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire_test_util::sample_request;
    use serde_json::json;

    #[test]
    fn payload_uses_typed_content_and_tool_calls_field() {
        let req = sample_request(lmhub_core::ReasoningLevel::Off);
        let p = build_payload(&req);
        assert_eq!(p["model"], json!("test-model"));
        assert_eq!(p["messages"][0]["role"], json!("system"));
        assert_eq!(p["messages"][0]["content"][0]["type"], json!("text"));
        assert_eq!(
            p["messages"][2]["tool_calls"][0]["function"]["name"],
            json!("read_file")
        );
        assert_eq!(p["tools"][0]["type"], json!("function"));
    }

    #[test]
    fn parses_cohere_response_shape() {
        let body = json!({
            "message": {"role": "assistant",
                        "content": [{"type": "text", "text": "hello"}],
                        "toolCalls": [{"id": "c1", "type": "function",
                                       "function": {"name": "f", "arguments": {"a": 1}}}]},
            "finish_reason": "TOOL_CALLS",
            "usage": {"tokens": {"input_tokens": 10, "output_tokens": 5}}
        });
        let resp = parse_response(&body, 10, Vec::new()).unwrap();
        assert_eq!(resp.text, "hello");
        assert_eq!(resp.tool_calls[0].arguments["a"], json!(1));
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
    }
}
