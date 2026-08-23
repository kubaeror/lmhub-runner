//! Azure OpenAI adapter: deployment-based URLs + `api-key` header.
//!
//! Base URL resolution: configured base_url wins, otherwise
//! `https://{AZURE_RESOURCE_NAME}.openai.azure.com` (or
//! `{...}.cognitiveservices.azure.com` for the second Azure entry).
//! Model ids are deployment names; there is no `/models` endpoint, so the
//! model list comes from Models.dev / local config (fallback chain).

use crate::http;
use crate::wire_openai::{self, OpenAiWireOpts};
use lmhub_core::{ChatRequest, ChatResponse, CoreError, Result};
use serde_json::Value;
use std::time::Instant;

pub const DEFAULT_API_VERSION: &str = "2024-10-21";

pub fn resolve_base(configured: Option<&str>, resource_env: &str) -> Result<String> {
    if let Some(base) = configured {
        return Ok(base.trim_end_matches('/').to_string());
    }
    let resource = std::env::var(resource_env).map_err(|_| {
        CoreError::Other(format!(
            "azure: set {resource_env} (resource name) or configure base_url in a provider TOML"
        ))
    })?;
    let host = match resource_env {
        "AZURE_COGNITIVE_SERVICES_RESOURCE_NAME" => "cognitiveservices.azure.com",
        _ => "openai.azure.com",
    };
    Ok(format!("https://{resource}.{host}"))
}

pub async fn chat(
    http_client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    request: &ChatRequest,
) -> Result<ChatResponse> {
    let url = format!(
        "{}/openai/deployments/{}/chat/completions?api-version={DEFAULT_API_VERSION}",
        base_url.trim_end_matches('/'),
        urlencode(request.model.as_str()),
    );
    let mut payload = wire_openai::build_chat_payload(
        request,
        OpenAiWireOpts {
            include_reasoning_effort: true,
        },
    );
    // Azure rejects `stream:false` being absent is fine; but reasoning_effort
    // uses the same field name. Nothing else to change.
    let _ = &mut payload;

    let headers = vec![("api-key".to_string(), api_key.to_string())];
    let headers_retry = headers.clone();
    let started = Instant::now();
    let body: Value = http::post_json(http_client, &url, headers, &payload).await?;
    let duration_ms = started.elapsed().as_millis() as u64;

    // Some deployments reject optional params; degrade once like native OpenAI.
    match wire_openai::parse_chat_response(&body, duration_ms, Vec::new()) {
        Ok(resp) => Ok(resp),
        Err(CoreError::Parse(msg)) => {
            if msg.contains("choices") && payload.get("tools").is_some() {
                let mut stripped = request.clone();
                stripped.tools.clear();
                stripped.reasoning = lmhub_core::ReasoningLevel::Off;
                let plain = wire_openai::build_chat_payload(
                    &stripped,
                    OpenAiWireOpts {
                        include_reasoning_effort: false,
                    },
                );
                let started2 = Instant::now();
                let body2 = http::post_json(http_client, &url, headers_retry, &plain).await?;
                return wire_openai::parse_chat_response(
                    &body2,
                    started2.elapsed().as_millis() as u64,
                    vec!["deployment rejected `tools`; retried without them".into()],
                );
            }
            Err(CoreError::Parse(msg))
        }
        Err(e) => Err(e),
    }
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

/// Streaming hits the same deployment endpoint with `stream: true`.
pub fn stream_url(base_url: &str, deployment: &str) -> String {
    format!(
        "{}/openai/deployments/{}/chat/completions?api-version={DEFAULT_API_VERSION}",
        base_url.trim_end_matches('/'),
        urlencode(deployment),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmhub_core::{ChatMessage, ReasoningLevel};

    #[test]
    fn builds_deployment_url() {
        std::env::remove_var("LMHUB_TEST_AZ_RES");
        assert!(resolve_base(Some("https://custom.example.com"), "LMHUB_TEST_AZ_RES").is_ok());
        std::env::set_var("LMHUB_TEST_AZ_RES", "my-resource");
        let base = resolve_base(None, "LMHUB_TEST_AZ_RES").unwrap();
        assert_eq!(base, "https://my-resource.openai.azure.com");
        let cs = resolve_base(None, "AZURE_COGNITIVE_SERVICES_RESOURCE_NAME");
        // env not set for that name → error mentioning it
        assert!(cs.is_err());
    }

    #[test]
    fn urlencodes_model_ids() {
        assert_eq!(urlencode("gpt-4o_2024.11~x/y"), "gpt-4o_2024.11~x%2Fy");
    }

    #[tokio::test]
    async fn payload_is_plain_openai_wire() {
        // Pure construction check: azure reuses the OpenAI serializer.
        let mut req = ChatRequest::new("gpt-5.1", "sys");
        req.messages.push(ChatMessage::user("hi"));
        let p = wire_openai::build_chat_payload(
            &req,
            OpenAiWireOpts {
                include_reasoning_effort: true,
            },
        );
        assert_eq!(p["model"], serde_json::json!("gpt-5.1"));
        assert_eq!(p["messages"][0]["role"], serde_json::json!("system"));
        let _ = ReasoningLevel::Off;
    }
}
