//! Native OpenAI adapter (api.openai.com, Chat Completions).

use crate::credentials;
use crate::http;
use crate::wire_common::merge_warnings;
use crate::wire_openai::{self, OpenAiWireOpts};
use async_trait::async_trait;
use lmhub_core::{AuthStore, ChatRequest, ChatResponse, CoreError, Provider, ProviderCaps, Result};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone)]
pub struct NativeOpenAiProvider {
    pub http: reqwest::Client,
    /// e.g. `https://api.openai.com/v1` or a proxy.
    pub base_url: String,
    pub id: String,
    pub name: String,
    pub api_key_env: String,
    /// Models.dev id for metadata lookup.
    pub models_dev_id: String,
    /// Shared credential store: auth.json wins over the env var.
    pub auth_store: Arc<Mutex<AuthStore>>,
}

impl NativeOpenAiProvider {
    pub fn standard(auth_store: Arc<Mutex<AuthStore>>) -> Arc<dyn Provider> {
        Arc::new(Self {
            http: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            id: "openai".into(),
            name: "OpenAI".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            models_dev_id: "openai".into(),
            auth_store,
        })
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
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

    async fn post_chat(&self, payload: &Value) -> Result<ChatResponse> {
        let key = self.key()?;
        let headers = vec![("authorization".to_string(), format!("Bearer {key}"))];
        let started = Instant::now();
        let body = http::post_json(&self.http, &self.chat_url(), headers, payload).await?;
        let duration = started.elapsed();
        wire_openai::parse_chat_response(&body, duration.as_millis() as u64, Vec::new())
    }
}

#[async_trait]
impl Provider for NativeOpenAiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.name
    }

    fn provider_type(&self) -> &str {
        "native-openai"
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
        let headers = vec![("authorization".to_string(), format!("Bearer {key}"))];
        let body = http::get_json(&self.http, &self.models_url(), headers).await?;
        Ok(Some(wire_openai::parse_models_list(&body)?))
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let mut warnings = Vec::new();

        // Attempt 1: full feature set.
        let payload = wire_openai::build_chat_payload(
            request,
            OpenAiWireOpts {
                include_reasoning_effort: true,
            },
        );
        match self.post_chat(&payload).await {
            Ok(resp) => Ok(merge_warnings(resp, warnings)),
            Err(CoreError::Provider(msg))
                if wire_openai::is_reasoning_rejection(&msg)
                    && request.reasoning != lmhub_core::ReasoningLevel::Off =>
            {
                // Attempt 2: model rejected the reasoning level — retry with
                // the level the provider suggests, or without reasoning.
                let mut retried = request.clone();
                retried.reasoning = wire_openai::suggested_reasoning_level(&msg)
                    .unwrap_or(lmhub_core::ReasoningLevel::Off);
                warnings.push(format!(
                    "provider rejected reasoning; retried with `{}`",
                    retried.reasoning.as_str()
                ));
                let plain = wire_openai::build_chat_payload(
                    &retried,
                    OpenAiWireOpts {
                        include_reasoning_effort: retried.reasoning
                            != lmhub_core::ReasoningLevel::Off,
                    },
                );
                self.post_chat(&plain)
                    .await
                    .map(|r| merge_warnings(r, warnings))
            }
            Err(CoreError::Provider(msg)) if crate::wire_common::is_tools_rejection(&msg) => {
                // Attempt 3: no tool support on this route — degrade to plain text.
                warnings.push(
                    "provider rejected `tools`; retried without tools (agent loop disabled)".into(),
                );
                let mut stripped = request.clone();
                stripped.tools.clear();
                stripped.reasoning = lmhub_core::ReasoningLevel::Off;
                let plain = wire_openai::build_chat_payload(
                    &stripped,
                    OpenAiWireOpts {
                        include_reasoning_effort: false,
                    },
                );
                self.post_chat(&plain)
                    .await
                    .map(|r| merge_warnings(r, warnings))
            }
            Err(e) => Err(e),
        }
    }

    async fn chat_stream(&self, request: &ChatRequest) -> Result<lmhub_core::ChatStream> {
        let key = self.key()?;
        let url = self.chat_url();
        let headers = vec![("authorization".to_string(), format!("Bearer {key}"))];
        crate::stream_runner::openai_sse(&self.http, url, headers, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire_test_util::{one_shot_server, sample_request};
    use lmhub_core::{ReasoningLevel, StoredCredential};

    const RESPONSE: &str = r#"{"id":"x","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

    fn store_with(secret: &str) -> Arc<std::sync::Mutex<AuthStore>> {
        let mut store = AuthStore::load(std::env::temp_dir().join("lmhub-test-auth-unused.json"));
        store.set_credential("openai", StoredCredential::api(secret));
        Arc::new(std::sync::Mutex::new(store))
    }

    fn provider(base_url: String, env_key: &str) -> NativeOpenAiProvider {
        provider_with(base_url, env_key, store_with("sk-stored-1234567890"))
    }

    fn provider_with(
        base_url: String,
        env_key: &str,
        auth_store: Arc<std::sync::Mutex<AuthStore>>,
    ) -> NativeOpenAiProvider {
        NativeOpenAiProvider {
            http: reqwest::Client::new(),
            base_url,
            id: "openai".into(),
            name: "OpenAI".into(),
            api_key_env: env_key.into(),
            models_dev_id: "openai".into(),
            auth_store,
        }
    }

    #[tokio::test]
    async fn stored_credential_is_used_without_env() {
        let env_key = "LMHUB_TEST_OPENAI_STORED_ONLY";
        std::env::remove_var(env_key);
        let (base, rx) = one_shot_server(200, "application/json", RESPONSE.into()).await;
        let p = provider(base, env_key);
        let resp = p.chat(&sample_request(ReasoningLevel::Off)).await.unwrap();
        assert_eq!(resp.text, "hi");
        let raw = rx.await.unwrap().to_ascii_lowercase();
        assert!(
            raw.contains("authorization: bearer sk-stored-1234567890"),
            "auth.json key must be sent: {raw}"
        );
    }

    #[tokio::test]
    async fn auth_json_wins_over_env() {
        let env_key = "LMHUB_TEST_OPENAI_PRECEDENCE";
        std::env::set_var(env_key, "env-value-1234567890");
        let (base, rx) = one_shot_server(200, "application/json", RESPONSE.into()).await;
        let p = provider(base, env_key);
        p.chat(&sample_request(ReasoningLevel::Off)).await.unwrap();
        let raw = rx.await.unwrap().to_ascii_lowercase();
        assert!(raw.contains("sk-stored-1234567890"));
        assert!(!raw.contains("env-value-1234567890"));
    }

    #[tokio::test]
    async fn missing_key_and_env_reports_missing_api_key() {
        let env_key = "LMHUB_TEST_OPENAI_NEVER_SET";
        std::env::remove_var(env_key);
        let (base, _rx) = one_shot_server(200, "application/json", RESPONSE.into()).await;
        let p = provider_with(
            base,
            env_key,
            Arc::new(std::sync::Mutex::new(AuthStore::load(
                std::env::temp_dir().join("lmhub-test-auth-empty.json"),
            ))),
        );
        let err = p
            .chat(&sample_request(ReasoningLevel::Off))
            .await
            .unwrap_err();
        match err {
            CoreError::MissingApiKey(msg) => assert!(
                msg.contains(env_key),
                "missing-key error should name the env var: {msg}"
            ),
            other => panic!("expected MissingApiKey, got {other:?}"),
        }
    }
}
