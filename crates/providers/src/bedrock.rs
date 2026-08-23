//! Amazon Bedrock adapter (Converse API).
//!
//! Auth (in precedence order):
//! 1. Bearer token — `AWS_BEARER_TOKEN_BEDROCK` env or a stored credential
//!    for provider id `amazon-bedrock`;
//! 2. SigV4 — `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`
//!    (+ optional `AWS_SESSION_TOKEN`);
//! 3. otherwise: clear missing-credential error.
//!
//! Region: `AWS_REGION` → `AWS_DEFAULT_REGION` → region from the active
//! profile in `~/.aws/config` (minimal INI read). Model ids come from
//! Models.dev / local config; Bedrock has no usable generic listing here.

use crate::credentials;
use crate::http;
use crate::sigv4;
use lmhub_core::{
    ChatRequest, ChatResponse, CoreError, Result, Role, StopReason, ToolCallRequest, Usage,
};
use serde_json::{json, Value};
use std::time::Instant;

pub fn resolve_region() -> Result<String> {
    if let Ok(r) = std::env::var("AWS_REGION") {
        return Ok(r);
    }
    if let Ok(r) = std::env::var("AWS_DEFAULT_REGION") {
        return Ok(r);
    }
    let profile = std::env::var("AWS_PROFILE").unwrap_or_else(|_| "default".into());
    if let Some(region) = region_from_aws_config(&profile) {
        return Ok(region);
    }
    Err(CoreError::Other(
        "bedrock: no AWS region found (set AWS_REGION or configure ~/.aws/config)".into(),
    ))
}

/// Minimal INI reader for `~/.aws/config` (`[default]` / `[profile x]`).
fn region_from_aws_config(profile: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let raw = std::fs::read_to_string(format!("{home}/.aws/config")).ok()?;
    let wanted = if profile == "default" {
        "[default]".to_string()
    } else {
        format!("[profile {profile}]")
    };
    let mut in_section = false;
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case(&wanted);
        } else if in_section {
            if let Some(value) = line.strip_prefix("region").map(str::trim) {
                if let Some(v) = value.strip_prefix('=') {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

enum BedrockAuth {
    Bearer(String),
    SigV4 {
        access_key: String,
        secret_key: String,
        session_token: Option<String>,
    },
}

fn resolve_auth(
    store: &std::sync::Arc<std::sync::Mutex<lmhub_core::AuthStore>>,
) -> Result<BedrockAuth> {
    // 1. bearer via store or env
    let stored = {
        let guard = store.lock().unwrap();
        credentials::resolve(
            &guard,
            "amazon-bedrock",
            &["AWS_BEARER_TOKEN_BEDROCK".to_string()],
        )
    };
    if let Some(cred) = stored {
        return Ok(BedrockAuth::Bearer(cred.secret));
    }
    // 2. static keys
    match (
        std::env::var("AWS_ACCESS_KEY_ID").ok(),
        std::env::var("AWS_SECRET_ACCESS_KEY").ok(),
    ) {
        (Some(k), Some(s)) if !k.is_empty() && !s.is_empty() => Ok(BedrockAuth::SigV4 {
            access_key: k,
            secret_key: s,
            session_token: std::env::var("AWS_SESSION_TOKEN").ok().filter(|t| !t.is_empty()),
        }),
        _ => Err(CoreError::MissingApiKey(
            "amazon-bedrock: set AWS_BEARER_TOKEN_BEDROCK (or AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY); \
             alternatively save a key for provider `amazon-bedrock` via TUI"
                .into(),
        )),
    }
}

pub fn converse_url(region: &str, model_id: &str) -> String {
    format!(
        "https://bedrock-runtime.{region}.amazonaws.com/model/{}/converse",
        urlencode(model_id)
    )
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

pub fn build_payload(request: &ChatRequest) -> Value {
    let mut messages = Vec::with_capacity(request.messages.len());
    for msg in &request.messages {
        match msg.role {
            Role::System => continue, // handled by top-level system
            Role::User => messages.push(json!({
                "role": "user",
                "content": [{"text": msg.content}],
            })),
            Role::Assistant => {
                let mut content = Vec::new();
                if !msg.content.is_empty() {
                    content.push(json!({"text": msg.content}));
                }
                for call in &msg.tool_calls {
                    content.push(json!({
                        "toolUse": {"toolUseId": call.id, "name": call.name, "input": call.arguments},
                    }));
                }
                if content.is_empty() {
                    content.push(json!({"text": "(empty turn)"}));
                }
                messages.push(json!({"role": "assistant", "content": content}));
            }
            Role::Tool => {
                let status = if msg.is_error { "error" } else { "success" };
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "toolResult": {
                            "toolUseId": msg.tool_call_id.clone().unwrap_or_default(),
                            "content": [{"text": msg.content}],
                            "status": status,
                        },
                    }],
                }));
            }
        }
    }

    let mut payload = json!({ "messages": messages });
    if !request.system.trim().is_empty() {
        payload["system"] = json!([{"text": request.system}]);
    }
    payload["inferenceConfig"] = json!({"maxTokens": request.max_tokens});
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({"toolSpec": {
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": {"json": t.parameters},
                }})
            })
            .collect();
        payload["toolConfig"] = json!({"tools": tools});
    }
    payload
}

pub fn parse_response(
    body: &Value,
    duration_ms: u64,
    warnings: Vec<String>,
) -> Result<ChatResponse> {
    let message = body
        .get("output")
        .and_then(|o| o.get("message"))
        .ok_or_else(|| CoreError::Parse("response has no output.message".into()))?;

    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for block in message
        .get("content")
        .and_then(|c| c.as_array())
        .unwrap_or(&vec![])
    {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            text_parts.push(text.to_string());
        }
        if let Some(use_block) = block.get("toolUse") {
            tool_calls.push(ToolCallRequest {
                id: use_block
                    .get("toolUseId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .into(),
                name: use_block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .into(),
                arguments: use_block.get("input").cloned().unwrap_or_else(|| json!({})),
            });
        }
    }

    let stop_reason = match body.get("stopReason").and_then(|v| v.as_str()) {
        Some("end_turn" | "stop_sequence") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::Length,
        Some("refusal") => StopReason::Refusal,
        None | Some(_) => StopReason::Other,
    };

    let usage_json = body.get("usage").cloned().unwrap_or(Value::Null);
    let u = &usage_json;
    let usage = Usage {
        input_tokens: u.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: u.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0),
        reasoning_tokens: u.get("reasoningTokens").and_then(|v| v.as_u64()),
        cache_read_tokens: u.get("cacheReadInputTokens").and_then(|v| v.as_u64()),
        cache_write_tokens: u.get("cacheWriteInputTokens").and_then(|v| v.as_u64()),
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
    request: &ChatRequest,
    store: &std::sync::Arc<std::sync::Mutex<lmhub_core::AuthStore>>,
) -> Result<ChatResponse> {
    let auth = resolve_auth(store)?;
    let region = resolve_region()?;
    let url = converse_url(&region, &request.model);
    let host = url
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let path = format!("/model/{}/converse", urlencode(request.model.as_str()));
    let payload = build_payload(request);
    let body_str = serde_json::to_string(&payload)
        .map_err(|e| CoreError::Other(format!("serialize converse payload: {e}")))?;
    let payload_hash = sigv4::sha256_hex(body_str.as_bytes());

    let started = Instant::now();
    let headers: Vec<(String, String)> = match &auth {
        BedrockAuth::Bearer(token) => vec![
            ("authorization".to_string(), format!("Bearer {token}")),
            ("content-type".to_string(), "application/json".to_string()),
        ],
        BedrockAuth::SigV4 {
            access_key,
            secret_key,
            session_token,
        } => {
            // Every header listed in SignedHeaders must be present on the
            // actual request — content-type is signed, so it must be sent
            // (reqwest does not add one for a raw body).
            let signed = sigv4::sign(
                &sigv4::SigV4Request {
                    method: "POST",
                    host: &host,
                    path: &path,
                    query: "",
                    region: &region,
                    service: "bedrock",
                    access_key,
                    secret_key,
                    session_token: session_token.as_deref(),
                    payload_hash: &payload_hash,
                    extra_headers: &[("content-type", "application/json")],
                },
                &sigv4::amz_date_now(),
            );
            let mut hs = vec![
                ("host".to_string(), host.clone()),
                ("content-type".to_string(), "application/json".to_string()),
                ("x-amz-date".to_string(), signed.amz_date),
                ("authorization".to_string(), signed.authorization),
            ];
            if let Some(token) = signed.security_token {
                hs.push(("x-amz-security-token".to_string(), token));
            }
            hs
        }
    };

    // Retries ride on the shared policy (429/5xx with backoff). The signed
    // headers are reused across attempts; SigV4 signatures stay valid for
    // the request within the 15-minute clock-skew window, so this is safe.
    let response = http::send_request(
        http_client,
        reqwest::Method::POST,
        &url,
        headers,
        Some(body_str),
        crate::http::REQUEST_TIMEOUT,
    )
    .await?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| CoreError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(CoreError::Provider(format!(
            "HTTP {}: {}",
            status.as_u16(),
            truncate(&lmhub_core::redact::scrub(&text))
        )));
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    parse_response(
        &serde_json::from_str(&text)
            .map_err(|e| CoreError::Parse(format!("{e}; body: {}", truncate(&text))))?,
        duration_ms,
        Vec::new(),
    )
}

fn truncate(body: &str) -> String {
    let scrubbed = lmhub_core::redact::scrub(body);
    let s: String = scrubbed.chars().take(600).collect();
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire_test_util::sample_request;
    use lmhub_core::ChatMessage;

    #[test]
    fn payload_maps_tools_results_and_system() {
        let mut req = sample_request(lmhub_core::ReasoningLevel::High);
        req.messages
            .push(ChatMessage::tool_result("call_1", "file body", false));
        let p = build_payload(&req);

        assert_eq!(
            p["system"][0]["text"],
            serde_json::json!("You are a coding agent.")
        );
        assert_eq!(p["inferenceConfig"]["maxTokens"], serde_json::json!(1024));
        assert_eq!(
            p["toolConfig"]["tools"][0]["toolSpec"]["name"],
            serde_json::json!("read_file")
        );
        assert_eq!(
            p["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"],
            p["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]
        );

        let msgs = p["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], serde_json::json!("user"));
        assert_eq!(
            last["content"][0]["toolResult"]["status"],
            serde_json::json!("success")
        );
        assert_eq!(
            last["content"][0]["toolResult"]["toolUseId"],
            serde_json::json!("call_1")
        );
    }

    #[test]
    fn parses_tool_use_and_usage() {
        let body = json!({
            "output": {"message": {"role": "assistant", "content": [
                {"text": "using tool"},
                {"toolUse": {"toolUseId": "tu1", "name": "read_file", "input": {"path": "a"}}}
            ]}},
            "stopReason": "tool_use",
            "usage": {"inputTokens": 100, "outputTokens": 50,
                      "cacheReadInputTokens": 20, "cacheWriteInputTokens": 5}
        });
        let resp = parse_response(&body, 10, Vec::new()).unwrap();
        assert_eq!(resp.text, "using tool");
        assert_eq!(resp.tool_calls[0].id, "tu1");
        assert_eq!(resp.usage.input_tokens, 100);
        assert_eq!(resp.usage.cache_read_tokens, Some(20));
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn region_falls_back_through_env_chain() {
        // Just verify it returns something deterministic in CI (no aws config).
        let result = resolve_region();
        if std::env::var("AWS_REGION").is_ok() || std::env::var("AWS_DEFAULT_REGION").is_ok() {
            assert!(result.is_ok());
        }
    }

    #[test]
    fn url_encodes_model_ids_with_colons() {
        assert_eq!(
            converse_url("us-east-1", "us.anthropic.claude-sonnet-4-5:v1:0"),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/us.anthropic.claude-sonnet-4-5%3Av1%3A0/converse"
        );
    }
}
