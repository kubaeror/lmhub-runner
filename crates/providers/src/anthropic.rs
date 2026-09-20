//! Native Anthropic adapter (api.anthropic.com, Messages API).

use crate::credentials;
use crate::http;
use crate::wire_anthropic::{self, AnthropicWireOpts};
use async_trait::async_trait;
use lmhub_core::{
    AuthStore, ChatRequest, ChatResponse, CoreError, Provider, ProviderCaps, ReasoningLevel, Result,
};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct NativeAnthropicProvider {
    pub http: reqwest::Client,
    pub base_url: String,
    pub id: String,
    pub name: String,
    pub api_key_env: String,
    pub models_dev_id: String,
    /// Shared credential store: auth.json wins over the env var.
    pub auth_store: Arc<Mutex<AuthStore>>,
}

impl NativeAnthropicProvider {
    pub fn standard(auth_store: Arc<Mutex<AuthStore>>) -> Arc<dyn Provider> {
        Arc::new(Self {
            http: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            id: "anthropic".into(),
            name: "Anthropic".into(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            models_dev_id: "anthropic".into(),
            auth_store,
        })
    }

    fn messages_url(&self) -> String {
        format!("{}/messages", self.base_url.trim_end_matches('/'))
    }

    fn models_url(&self) -> String {
        format!("{}/models", self.base_url.trim_end_matches('/'))
    }

    /// auth.json → env → `missing_api_key`, resolved per request.
    fn key(&self) -> Result<String> {
        credentials::require_shared(
            &self.auth_store,
            &self.id,
            std::slice::from_ref(&self.api_key_env),
        )
    }

    async fn post_messages(&self, payload: &serde_json::Value) -> Result<ChatResponse> {
        let key = self.key()?;
        let headers = vec![
            ("x-api-key".to_string(), key),
            (
                "anthropic-version".to_string(),
                ANTHROPIC_VERSION.to_string(),
            ),
        ];
        let started = Instant::now();
        let body = http::post_json(&self.http, &self.messages_url(), headers, payload).await?;
        let duration = started.elapsed();
        wire_anthropic::parse_chat_response(&body, duration.as_millis() as u64, Vec::new())
    }
}

#[async_trait]
impl Provider for NativeAnthropicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.name
    }

    fn provider_type(&self) -> &str {
        "native-anthropic"
    }

    fn api_key_env(&self) -> &str {
        &self.api_key_env
    }

    fn models_dev_hint(&self) -> Option<&str> {
        Some(&self.models_dev_id)
    }

    fn supports_model_listing(&self) -> bool {
        true
    }

    fn caps(&self) -> ProviderCaps {
        ProviderCaps {
            tool_calls: true,
            reasoning: true,
            prompt_caching: true,
        }
    }

    async fn list_models_api(&self) -> Result<Option<Vec<String>>> {
        let key = self.key()?;
        let headers = vec![
            ("x-api-key".to_string(), key),
            (
                "anthropic-version".to_string(),
                ANTHROPIC_VERSION.to_string(),
            ),
        ];
        let body = http::get_json(&self.http, &self.models_url(), headers).await?;
        Ok(Some(wire_anthropic::parse_models_list(&body)?))
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let mut warnings = Vec::new();

        // Attempt 1: thinking enabled when requested.
        let payload = wire_anthropic::build_chat_payload(
            request,
            AnthropicWireOpts {
                supports_thinking: true,
            },
        );
        match self.post_messages(&payload).await {
            Ok(resp) => Ok(resp),
            Err(CoreError::Provider(msg))
                if is_thinking_rejection(&msg) && request.reasoning != ReasoningLevel::Off =>
            {
                warnings.push("provider rejected `thinking`; retried without it".into());
                let plain = wire_anthropic::build_chat_payload(
                    request,
                    AnthropicWireOpts {
                        supports_thinking: false,
                    },
                );
                self.post_messages(&plain).await.map(|mut r| {
                    r.warnings.extend(warnings.iter().cloned());
                    r
                })
            }
            Err(CoreError::Provider(msg)) if is_tools_rejection(&msg) => {
                warnings.push("provider rejected `tools`; retried without tools".into());
                let mut stripped = request.clone();
                stripped.tools.clear();
                stripped.reasoning = ReasoningLevel::Off;
                let plain = wire_anthropic::build_chat_payload(
                    &stripped,
                    AnthropicWireOpts {
                        supports_thinking: false,
                    },
                );
                self.post_messages(&plain).await.map(|mut r| {
                    r.warnings.extend(warnings.iter().cloned());
                    r
                })
            }
            Err(e) => Err(e),
        }
    }

    async fn chat_stream(&self, request: &ChatRequest) -> Result<lmhub_core::ChatStream> {
        let key = self.key()?;
        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));
        let headers = vec![
            ("x-api-key".to_string(), key),
            (
                "anthropic-version".to_string(),
                crate::anthropic::ANTHROPIC_VERSION.into(),
            ),
        ];
        crate::stream_runner::anthropic_sse(&self.http, url, headers, request).await
    }
}

fn is_thinking_rejection(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("thinking") && (m.contains("unsupported") || m.contains("not supported"))
}

fn is_tools_rejection(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("'tools'") || (m.contains("tools") && m.contains("unsupported"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire_test_util::{one_shot_server, sample_request};
    use lmhub_core::{ReasoningLevel, StoredCredential};

    const RESPONSE: &str = r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}],"model":"test","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

    #[tokio::test]
    async fn stored_credential_is_used_without_env() {
        let env_key = "LMHUB_TEST_ANTHROPIC_STORED_ONLY";
        std::env::remove_var(env_key);
        let mut store = AuthStore::load(std::env::temp_dir().join("lmhub-test-auth-unused.json"));
        store.set_credential(
            "anthropic",
            StoredCredential::api("sk-ant-stored-1234567890"),
        );
        let (base, rx) = one_shot_server(200, "application/json", RESPONSE.into()).await;
        let p = NativeAnthropicProvider {
            http: reqwest::Client::new(),
            base_url: base,
            id: "anthropic".into(),
            name: "Anthropic".into(),
            api_key_env: env_key.into(),
            models_dev_id: "anthropic".into(),
            auth_store: Arc::new(Mutex::new(store)),
        };
        let resp = p.chat(&sample_request(ReasoningLevel::Off)).await.unwrap();
        assert_eq!(resp.text, "hi");
        let raw = rx.await.unwrap().to_ascii_lowercase();
        assert!(
            raw.contains("x-api-key: sk-ant-stored-1234567890"),
            "auth.json key must be sent: {raw}"
        );
    }
}
