//! Native OpenAI adapter (api.openai.com, Chat Completions).

use crate::http;
use crate::wire_openai::{self, OpenAiWireOpts};
use async_trait::async_trait;
use lmhub_core::{ChatRequest, ChatResponse, CoreError, Provider, ProviderCaps, Result};
use serde_json::Value;
use std::sync::Arc;
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
}

impl NativeOpenAiProvider {
    pub fn standard() -> Arc<dyn Provider> {
        Arc::new(Self {
            http: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            id: "openai".into(),
            name: "OpenAI".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            models_dev_id: "openai".into(),
        })
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn models_url(&self) -> String {
        format!("{}/models", self.base_url.trim_end_matches('/'))
    }

    async fn post_chat(&self, payload: &Value) -> Result<ChatResponse> {
        let key = std::env::var(&self.api_key_env)
            .map_err(|_| CoreError::MissingApiKey(self.api_key_env.clone()))?;
        let headers = vec![("authorization".to_string(), format!("Bearer {key}"))];
        let started = Instant::now();
        let body = http::post_json(&self.http, &self.chat_url(), headers, payload).await?;
        let duration = started.elapsed();
        wire_openai::parse_chat_response(&body, duration.as_millis() as u64, Vec::new())
    }
}

fn merge_warnings(mut resp: ChatResponse, warnings: Vec<String>) -> ChatResponse {
    let mut all = warnings;
    all.append(&mut resp.warnings);
    resp.warnings = all;
    resp
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
        let key = std::env::var(&self.api_key_env)
            .map_err(|_| CoreError::MissingApiKey(self.api_key_env.clone()))?;
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
            Err(CoreError::Provider(msg)) if is_tools_rejection(&msg) => {
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
        let key = std::env::var(&self.api_key_env)
            .map_err(|_| CoreError::MissingApiKey(self.api_key_env.clone()))?;
        let url = self.chat_url();
        let headers = vec![("authorization".to_string(), format!("Bearer {key}"))];
        crate::stream_runner::openai_sse(&self.http, url, headers, request).await
    }
}

fn is_tools_rejection(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("'tools'") || m.contains("\"tools\"") || m.contains("tool use is not supported")
}
