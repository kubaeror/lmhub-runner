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

use crate::credentials;
use crate::http;
use crate::wire_anthropic::{self, AnthropicWireOpts};
use crate::wire_openai::{self, OpenAiWireOpts};
use async_trait::async_trait;
use lmhub_core::{
    AuthStore, ChatRequest, ChatResponse, CoreError, LocalModel, Provider, ProviderCaps, Result,
};
use serde::Deserialize;
use std::path::Path;
use std::sync::{Arc, Mutex};

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
    /// Explicit opt-in for plain-`http://` base URLs on non-local hosts.
    pub allow_insecure: bool,
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
            allow_insecure: false,
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
        if self.base_url.starts_with("http://")
            && !is_local_http_host(&self.base_url)
            && !self.allow_insecure
        {
            return Err(CoreError::Other(format!(
                "provider `{}` uses a plain-http base_url for a non-local host; \
                 use https:// or set allow_insecure = true explicitly",
                self.id
            )));
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

/// True when a plain-http base URL points at the local machine (allowed
/// without `allow_insecure` — local runtimes like Ollama/LM Studio).
fn is_local_http_host(base_url: &str) -> bool {
    let host_port = base_url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("");
    let host = host_port.split(':').next().unwrap_or("");
    matches!(
        host,
        "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]" | "::1"
    )
}

/// Config-driven provider speaking either the OpenAI-compatible or the
/// Anthropic-compatible protocol. This is what makes custom providers
/// possible **without any change to the runner core**.
pub struct CustomProvider {
    cfg: CustomProviderConfig,
    http: reqwest::Client,
    /// Shared credential store: auth.json wins over `api_key_env`.
    auth_store: Arc<Mutex<AuthStore>>,
}

impl CustomProvider {
    pub fn from_config(
        cfg: CustomProviderConfig,
        auth_store: Arc<Mutex<AuthStore>>,
    ) -> Arc<dyn Provider> {
        Arc::new(Self {
            cfg,
            http: reqwest::Client::new(),
            auth_store,
        })
    }

    /// auth.json → env → `missing_api_key`, resolved per request so a key
    /// entered in the TUI takes effect without a restart.
    fn key(&self) -> Result<String> {
        credentials::require_shared(
            &self.auth_store,
            &self.cfg.id,
            std::slice::from_ref(&self.cfg.api_key_env),
        )
    }

    fn auth_headers(&self) -> Result<Vec<(String, String)>> {
        let key = self.key()?;
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
        // Honour the declared capabilities: never send tools or cache
        // controls the config says are unsupported.
        let mut warnings: Vec<String> = Vec::new();
        let mut effective = request.clone();
        if !self.cfg.supports_prompt_caching {
            effective.enable_prompt_cache = false;
        }
        if !self.cfg.supports_tool_calls && !effective.tools.is_empty() {
            effective.tools.clear();
            warnings.push(format!(
                "provider `{}` declares no tool support; tools stripped",
                self.cfg.id
            ));
        }
        let mut resp = match self.cfg.api_type.unwrap_or(ApiType::OpenAiCompatible) {
            ApiType::OpenAiCompatible => {
                // Reasoning param only when configured as supported.
                let payload = wire_openai::build_chat_payload(
                    &effective,
                    OpenAiWireOpts {
                        include_reasoning_effort: self.cfg.supports_reasoning,
                    },
                );
                self.post_chat(&payload).await?
            }
            ApiType::AnthropicCompatible => {
                let payload = wire_anthropic::build_chat_payload(
                    &effective,
                    AnthropicWireOpts {
                        supports_thinking: self.cfg.supports_reasoning,
                    },
                );
                self.post_chat(&payload).await?
            }
        };
        let mut all = warnings;
        all.append(&mut resp.warnings);
        resp.warnings = all;
        Ok(resp)
    }
}

/// Load every valid `*.toml` in `dir` as a provider. Broken files are
/// reported in the returned error list — one bad file must not kill startup.
pub fn load_providers_from_dir(
    dir: &Path,
    auth_store: Arc<Mutex<AuthStore>>,
) -> (Vec<Arc<dyn Provider>>, Vec<String>) {
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
            Ok(cfg) => providers.push(CustomProvider::from_config(cfg, Arc::clone(&auth_store))),
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
    fn plain_http_requires_localhost_or_opt_in() {
        // Local runtimes (Ollama & co) keep working over http.
        let local = SAMPLE.replace("https://api.example.com/v1", "http://localhost:11434/v1");
        assert!(CustomProviderConfig::parse_toml(&local).is_ok());
        let loopback = SAMPLE.replace("https://api.example.com/v1", "http://127.0.0.1:8080/v1");
        assert!(CustomProviderConfig::parse_toml(&loopback).is_ok());

        // Remote plain-http is refused...
        let remote = SAMPLE.replace("https://api.example.com/v1", "http://api.example.com/v1");
        let err = CustomProviderConfig::parse_toml(&remote).unwrap_err();
        assert!(err.to_string().contains("allow_insecure"), "{err}");

        // ...unless explicitly opted in.
        let opted_in = remote.replace(
            "supports_tool_calls = true",
            "supports_tool_calls = true\nallow_insecure = true",
        );
        assert!(CustomProviderConfig::parse_toml(&opted_in).is_ok());
    }

    #[tokio::test]
    async fn tool_and_cache_capabilities_are_enforced() {
        use crate::wire_test_util::{one_shot_server, sample_request};
        let (base, rx) = one_shot_server(
            200,
            "application/json",
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#
                .to_string(),
        )
        .await;
        let cfg = CustomProviderConfig {
            id: "my-provider".into(),
            base_url: base,
            api_key_env: "LMHUB_TEST_CUSTOM_CAPS".into(),
            supports_tool_calls: false,
            supports_prompt_caching: false,
            ..Default::default()
        };
        let mut store = AuthStore::load(std::env::temp_dir().join("lmhub-test-auth-caps.json"));
        store.set_credential(
            "my-provider",
            lmhub_core::StoredCredential::api("sk-custom-caps-1234567890"),
        );
        let store = std::sync::Arc::new(std::sync::Mutex::new(store));
        let provider = CustomProvider::from_config(cfg, store);
        let mut req = sample_request(lmhub_core::ReasoningLevel::Off);
        req.enable_prompt_cache = true; // must be turned off
        let resp = provider.chat(&req).await.unwrap();
        assert!(
            resp.warnings.iter().any(|w| w.contains("tool")),
            "{:?}",
            resp.warnings
        );
        let raw_request = rx.await.unwrap();
        assert!(
            !raw_request.contains("\"tools\""),
            "tools must be stripped: {raw_request}"
        );
        assert!(
            !raw_request.contains("cache_control"),
            "cache control must not be sent: {raw_request}"
        );
    }

    #[test]
    fn anthropic_compat_defaults_to_messages_path() {
        let raw = SAMPLE.replace("openai-compatible", "anthropic-compatible");
        let cfg = CustomProviderConfig::parse_toml(&raw).unwrap();
        assert_eq!(cfg.chat_endpoint(), "https://api.example.com/v1/messages");
        assert_eq!(cfg.api_type, Some(ApiType::AnthropicCompatible));
    }

    #[tokio::test]
    async fn stored_credential_is_used_without_env() {
        use crate::wire_test_util::{one_shot_server, sample_request};
        use lmhub_core::{ReasoningLevel, StoredCredential};

        let env_key = "LMHUB_TEST_CUSTOM_STORED_ONLY";
        std::env::remove_var(env_key);
        let mut store = AuthStore::load(std::env::temp_dir().join("lmhub-test-auth-unused.json"));
        store.set_credential(
            "my-provider",
            StoredCredential::api("sk-custom-stored-1234567890"),
        );
        let (base, rx) = one_shot_server(
            200,
            "application/json",
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#
                .into(),
        )
        .await;
        let cfg = CustomProviderConfig {
            id: "my-provider".into(),
            base_url: base,
            api_key_env: env_key.into(),
            ..Default::default()
        };
        let p = CustomProvider::from_config(cfg, std::sync::Arc::new(std::sync::Mutex::new(store)));
        let resp = p.chat(&sample_request(ReasoningLevel::Off)).await.unwrap();
        assert_eq!(resp.text, "hi");
        let raw = rx.await.unwrap().to_ascii_lowercase();
        assert!(
            raw.contains("authorization: bearer sk-custom-stored-1234567890"),
            "auth.json key must be sent: {raw}"
        );
    }
}
