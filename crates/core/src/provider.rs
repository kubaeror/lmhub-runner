use crate::chat::{ChatRequest, ChatResponse};
use crate::error::Result;
use crate::model::{Capabilities, ModelListSource};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Feature flags of a whole provider/adapter.
///
/// Defaults are fail-safe (all false): adapters must opt in to features they
/// actually implement, so a forgetful adapter never over-claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProviderCaps {
    pub tool_calls: bool,
    pub reasoning: bool,
    pub prompt_caching: bool,
}

impl From<ProviderCaps> for Capabilities {
    fn from(c: ProviderCaps) -> Self {
        Self {
            tool_call: c.tool_calls,
            reasoning: c.reasoning,
            prompt_caching: c.prompt_caching,
        }
    }
}

/// A model statically declared in a custom provider config file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalModel {
    pub id: String,
    pub name: Option<String>,
    pub family: Option<String>,
    pub reasoning: bool,
    pub tool_call: bool,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
}

/// Modular provider adapter. Implement this trait to add a new provider
/// without touching the runner core.
///
/// Implementations must never log or persist API keys.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable identifier used in configs and output paths.
    fn id(&self) -> &str;
    /// Human readable name shown in the TUI.
    fn display_name(&self) -> &str;
    /// Adapter type label, e.g. `native-anthropic`, `openai-compatible`.
    fn provider_type(&self) -> &str;
    /// Name of the environment variable holding the API key.
    fn api_key_env(&self) -> &str;

    /// All environment variable names that could hold this provider's key.
    fn env_keys(&self) -> Vec<String> {
        vec![self.api_key_env().to_string()]
    }

    /// False for local runtimes that need no credentials at all.
    fn requires_credentials(&self) -> bool {
        true
    }

    /// Models statically configured for this provider (custom providers only).
    fn local_models(&self) -> &[LocalModel] {
        &[]
    }

    /// Provider id under which Models.dev metadata/prices should be looked up.
    fn models_dev_hint(&self) -> Option<&str> {
        None
    }

    /// Whether the upstream API exposes a model listing endpoint.
    fn supports_model_listing(&self) -> bool {
        false
    }

    fn caps(&self) -> ProviderCaps;

    /// Query the provider's own `/models`-style endpoint.
    /// `Ok(None)` means "no endpoint available" — callers fall back to
    /// Models.dev / local config. Never returns fabricated models.
    async fn list_models_api(&self) -> Result<Option<Vec<String>>>;

    /// One chat completion turn with tool definitions.
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse>;

    /// Streaming variant. The returned stream ALWAYS ends with exactly one
    /// [`crate::chat::ChatStreamItem::Completed`] holding the assembled
    /// response. The default implementation delegates to [`Self::chat`] —
    /// protocols without native SSE keep working unchanged.
    async fn chat_stream(&self, request: &ChatRequest) -> Result<crate::chat::ChatStream> {
        let response = self.chat(request).await?;
        Ok(crate::chat::ChatRequest::completed_stream(response))
    }

    /// Default provenance label when the API listing succeeds.
    fn api_source(&self) -> ModelListSource {
        ModelListSource::ProviderApi
    }
}
