//! Custom provider definitions from `providers/*.toml`.
//!
//! Example:
//!
//! ```toml
//! id = "my-provider"
//! name = "My Provider"
//! api_type = "openai-compatible"          # or "anthropic-compatible"
//! base_url = "https://api.example.com/v1"
//! api_key_env = "MY_PROVIDER_API_KEY"
//! models_path = "/models"                 # optional; omit to skip listing
//! chat_path = "/chat/completions"         # optional override
//!
//! supports_tool_calls = true
//! supports_reasoning = true
//! supports_prompt_caching = false
//! models_dev_provider = "hpc-ai"          # optional pricing/metadata mapping
//!
//! [[models]]
//! id = "model-a"
//! name = "Model A"
//! family = "GLM"
//! reasoning = true
//! tool_call = true
//! context_window = 128000
//! max_output = 8192
//! ```

use crate::http;
use crate::wire_anthropic::{self, AnthropicWireOpts};
use crate::wire_openai::{self, OpenAiWireOpts};
use async_trait::async_trait;
use lmhub_core::{
    ChatRequest, ChatResponse, CoreError, LocalModel, Provider, ProviderCaps, Result,
};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum ApiType {
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
    #[serde(rename = "anthropic-compatible")]
    AnthropicCompatible,
}

impl ApiType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::AnthropicCompatible => "anthropic-compatible",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CustomProviderConfig {
    pub id: String,
    pub name: Option<String>,
    pub api_type: Option<ApiType>,
    pub base_url: String,
    pub api_key_env: String,
    /// Path appended to base_url for model listing; `None` disables listing.
    pub models_path: Option<String>,
    /// Path appended to base_url for chat; defaults per api_type.
    pub chat_path: Option<String>,
    pub supports_tool_calls: bool,
    pub supports_reasoning: bool,
    pub supports_prompt_caching: bool,
    pub models_dev_provider: Option<String>,
    #[serde(default)]
    pub models: Vec<LocalModel>,
}

impl Default for CustomProviderConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: None,
            api_type: Some(ApiType::OpenAiCompatible),
            base_url: String::new(),
            api_key_env: String::new(),
            models_path: None,
            chat_path: None,
            supports_tool_calls: false,
            supports_reasoning: false,
            supports_prompt_caching: false,
            models_dev_provider: None,
            models: Vec::new(),
        }
    }
}

impl CustomProviderConfig {
    pub fn parse_toml(raw: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(raw)
            .map_err(|e| CoreError::Other(format!("invalid provider TOML: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty() {
            return Err(CoreError::Other("provider `id` is required".into()));
        }
        if !self
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(CoreError::Other(
                "provider `id` must be lowercase [a-z0-9_-]".into(),
            ));
        }
        if !self.base_url.starts_with("http://") && !self.base_url.starts_with("https://") {
            return Err(CoreError::Other(
                "provider `base_url` must start with http(s)://".into(),
            ));
        }
        if self.api_key_env.is_empty() {
            return Err(CoreError::Other(
                "provider `api_key_env` is required".into(),
            ));
        }
        Ok(())
    }

    fn chat_endpoint(&self) -> String {
        let default_path = match self.api_type.unwrap_or(ApiType::OpenAiCompatible) {
            ApiType::OpenAiCompatible => "/chat/completions",
            ApiType::AnthropicCompatible => "/messages",
        };
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            self.chat_path
                .as_deref()
                .unwrap_or(default_path)
                .trim_start_matches('/')
        )
    }

    fn models_endpoint(&self) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            self.models_path
                .as_deref()
                .unwrap_or("/models")
                .trim_start_matches('/')
        )
    }
}

/// Config-driven provider speaking either the OpenAI-compatible or the
/// Anthropic-compatible protocol. This is what makes custom providers
/// possible **without any change to the runner core**.
pub struct CustomProvider {
    cfg: CustomProviderConfig,
    http: reqwest::Client,
}

impl CustomProvider {
    pub fn from_config(cfg: CustomProviderConfig) -> Arc<dyn Provider> {
        Arc::new(Self {
            cfg,
            http: reqwest::Client::new(),
        })
    }

    fn auth_headers(&self) -> Result<Vec<(String, String)>> {
        let key = std::env::var(&self.cfg.api_key_env)
            .map_err(|_| CoreError::MissingApiKey(self.cfg.api_key_env.clone()))?;
        Ok(
            match self.cfg.api_type.unwrap_or(ApiType::OpenAiCompatible) {
                ApiType::OpenAiCompatible => {
                    vec![("authorization".to_string(), format!("Bearer {key}"))]
                }
                ApiType::AnthropicCompatible => vec![
                    ("x-api-key".to_string(), key),
                    ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ],
            },
        )
    }

    async fn post_chat(&self, payload: &serde_json::Value) -> Result<ChatResponse> {
        let headers = self.auth_headers()?;
        let started = std::time::Instant::now();
        let body = http::post_json(&self.http, &self.cfg.chat_endpoint(), headers, payload).await?;
        let duration_ms = started.elapsed().as_millis() as u64;
        match self.cfg.api_type.unwrap_or(ApiType::OpenAiCompatible) {
            ApiType::OpenAiCompatible => {
                wire_openai::parse_chat_response(&body, duration_ms, Vec::new())
            }
            ApiType::AnthropicCompatible => {
                wire_anthropic::parse_chat_response(&body, duration_ms, Vec::new())
            }
        }
    }
}

#[async_trait]
impl Provider for CustomProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn display_name(&self) -> &str {
        self.cfg.name.as_deref().unwrap_or(&self.cfg.id)
    }

    fn provider_type(&self) -> &str {
        self.cfg
            .api_type
            .unwrap_or(ApiType::OpenAiCompatible)
            .as_str()
    }

    fn api_key_env(&self) -> &str {
        &self.cfg.api_key_env
    }

    fn local_models(&self) -> &[LocalModel] {
        &self.cfg.models
    }

    fn models_dev_hint(&self) -> Option<&str> {
        self.cfg.models_dev_provider.as_deref()
    }

    fn supports_model_listing(&self) -> bool {
        self.cfg.models_path.is_some()
    }

    fn caps(&self) -> ProviderCaps {
        ProviderCaps {
            tool_calls: self.cfg.supports_tool_calls,
            reasoning: self.cfg.supports_reasoning,
            prompt_caching: self.cfg.supports_prompt_caching,
        }
    }

    async fn list_models_api(&self) -> Result<Option<Vec<String>>> {
        let Some(_) = self.cfg.models_path else {
            return Ok(None);
        };
        let headers = self.auth_headers()?;
        let body = http::get_json(&self.http, &self.cfg.models_endpoint(), headers).await?;
        // Both compatible dialects use the same data[] shape.
        Ok(Some(wire_openai::parse_models_list(&body)?))
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse> {
        match self.cfg.api_type.unwrap_or(ApiType::OpenAiCompatible) {
            ApiType::OpenAiCompatible => {
                // Reasoning param only when configured as supported.
                let payload = wire_openai::build_chat_payload(
                    request,
                    OpenAiWireOpts {
                        include_reasoning_effort: self.cfg.supports_reasoning,
                    },
                );
                self.post_chat(&payload).await
            }
            ApiType::AnthropicCompatible => {
                let payload = wire_anthropic::build_chat_payload(
                    request,
                    AnthropicWireOpts {
                        supports_thinking: self.cfg.supports_reasoning,
                    },
                );
                self.post_chat(&payload).await
            }
        }
    }
}

/// Load every valid `*.toml` in `dir` as a provider. Broken files are
/// reported in the returned error list — one bad file must not kill startup.
pub fn load_providers_from_dir(dir: &Path) -> (Vec<Arc<dyn Provider>>, Vec<String>) {
    let mut providers = Vec::new();
    let mut errors = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return (providers, errors), // dir absent: fine
    };
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "toml").unwrap_or(false))
        .collect();
    paths.sort();
    for path in paths {
        match std::fs::read_to_string(&path)
            .map_err(CoreError::Io)
            .and_then(|raw| CustomProviderConfig::parse_toml(&raw))
        {
            Ok(cfg) => providers.push(CustomProvider::from_config(cfg)),
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    (providers, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
id = "my-provider"
name = "My Provider"
api_type = "openai-compatible"
base_url = "https://api.example.com/v1"
api_key_env = "MY_PROVIDER_API_KEY"
supports_tool_calls = true
supports_reasoning = true
models_dev_provider = "hpc-ai"

[[models]]
id = "model-a"
family = "GLM"
reasoning = true
tool_call = true
"#;

    #[test]
    fn parses_and_validates_sample() {
        let cfg = CustomProviderConfig::parse_toml(SAMPLE).unwrap();
        assert_eq!(cfg.id, "my-provider");
        assert_eq!(cfg.api_type, Some(ApiType::OpenAiCompatible));
        assert_eq!(
            cfg.chat_endpoint(),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(cfg.models_endpoint(), "https://api.example.com/v1/models");
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].family.as_deref(), Some("GLM"));
    }

    #[test]
    fn rejects_bad_base_url() {
        let raw = SAMPLE.replace("https://api.example.com/v1", "not-a-url");
        assert!(CustomProviderConfig::parse_toml(&raw).is_err());
    }

    #[test]
    fn anthropic_compat_defaults_to_messages_path() {
        let raw = SAMPLE.replace("openai-compatible", "anthropic-compatible");
        let cfg = CustomProviderConfig::parse_toml(&raw).unwrap();
        assert_eq!(cfg.chat_endpoint(), "https://api.example.com/v1/messages");
        assert_eq!(cfg.api_type, Some(ApiType::AnthropicCompatible));
    }
}
