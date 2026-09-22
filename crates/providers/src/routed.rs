//! `RoutedProvider`: one generic `Provider` implementation covering the long
//! tail of catalog providers by dispatching on [`ProtocolKind`].
//!
//! Credential resolution (auth.json → env) happens per request, so keys can
//! be added via the TUI without restarting.

use crate::azure;
use crate::bedrock;
use crate::cohere;
use crate::copilot;
use crate::credentials::{self};
use crate::gemini;
use crate::http;
use crate::known::ProtocolKind;
use crate::preauth;
use crate::stream_runner;
use crate::vertex;
use crate::wire_openai::{self, OpenAiWireOpts};
use async_trait::async_trait;
use lmhub_core::{
    AuthStore, ChatRequest, ChatResponse, CoreError, LocalModel, Provider, ProviderCaps, Result,
};
use std::sync::Arc;

/// Watsonx default host/version when the env does not override them.
const WATSONX_DEFAULT_HOST: &str = "https://eu-de.ml.cloud.ibm.com";
const WATSONX_API_VERSION: &str = "2024-05-31";

pub struct RoutedProvider {
    pub id: String,
    pub display_name: String,
    pub protocol: ProtocolKind,
    /// Static base URL from the catalog/TOML; dynamic protocols resolve
    /// their own URLs at call time.
    pub base_url: Option<String>,
    /// Credential-bearing env vars only (catalog `key_envs`).
    pub key_envs: Vec<String>,
    pub local_models: Vec<LocalModel>,
    pub models_dev_id: String,
    pub requires_key: bool,
    http: reqwest::Client,
    auth_store: Arc<std::sync::Mutex<AuthStore>>,
}

impl RoutedProvider {
    /// Build an `Arc<dyn Provider>` for a catalog entry.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: String,
        display_name: String,
        protocol: ProtocolKind,
        base_url: Option<String>,
        key_envs: Vec<String>,
        models_dev_id: String,
        requires_key: bool,
        auth_store: Arc<std::sync::Mutex<AuthStore>>,
        local_models: Vec<LocalModel>,
    ) -> Arc<dyn Provider> {
        Arc::new(Self {
            id,
            display_name,
            protocol,
            base_url,
            key_envs,
            local_models,
            models_dev_id,
            requires_key,
            http: reqwest::Client::new(),
            auth_store,
        })
    }

    fn store(&self) -> std::sync::MutexGuard<'_, AuthStore> {
        self.auth_store.lock().unwrap()
    }

    async fn key(&self) -> Result<String> {
        if !self.requires_key {
            return Ok(String::new());
        }
        // Catalog regression guard: `key_envs` must never contain display-only
        // vars (that bug made `AZURE_RESOURCE_NAME` an "API key").
        debug_assert!(
            self.key_envs
                .iter()
                .all(|k| crate::known::is_secret_like_env_name(k)),
            "provider `{}` passed non-credential env vars to key resolution: {:?}",
            self.id,
            self.key_envs
        );
        let resolved = {
            let store = self.store();
            credentials::resolve(&store, &self.id, &self.key_envs)
        };
        resolved
            .map(|c| c.secret)
            .ok_or_else(|| credentials::missing_error(&self.id, &self.key_envs))
    }

    async fn chat_openai_wire(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        request: &ChatRequest,
    ) -> Result<ChatResponse> {
        let started = std::time::Instant::now();
        let payload = wire_openai::build_chat_payload(
            request,
            OpenAiWireOpts {
                include_reasoning_effort: true,
            },
        );
        let body = match http::post_json(&self.http, &url, headers.clone(), &payload).await {
            Ok(b) => b,
            Err(CoreError::Provider(msg))
                if wire_openai::is_reasoning_rejection(&msg)
                    && request.reasoning != lmhub_core::ReasoningLevel::Off =>
            {
                // Model rejected the reasoning level. Prefer the level the
                // provider names ("please use low, high, or max"); fall back
                // to stripping reasoning entirely.
                let mut retried = request.clone();
                retried.reasoning = wire_openai::suggested_reasoning_level(&msg)
                    .unwrap_or(lmhub_core::ReasoningLevel::Off);
                let plain = wire_openai::build_chat_payload(
                    &retried,
                    OpenAiWireOpts {
                        include_reasoning_effort: retried.reasoning
                            != lmhub_core::ReasoningLevel::Off,
                    },
                );
                http::post_json(&self.http, &url, headers, &plain).await?
            }
            Err(e) => return Err(e),
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        wire_openai::parse_chat_response(&body, duration_ms, Vec::new())
    }
}

#[async_trait]
impl Provider for RoutedProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn provider_type(&self) -> &str {
        self.protocol.provider_type()
    }

    fn api_key_env(&self) -> &str {
        self.key_envs.first().map(String::as_str).unwrap_or("-")
    }

    fn env_keys(&self) -> Vec<String> {
        self.key_envs.clone()
    }

    fn requires_credentials(&self) -> bool {
        self.requires_key
    }

    fn local_models(&self) -> &[LocalModel] {
        &self.local_models
    }

    fn models_dev_hint(&self) -> Option<&str> {
        Some(&self.models_dev_id)
    }

    fn supports_model_listing(&self) -> bool {
        matches!(
            self.protocol,
            ProtocolKind::OpenAiCompat | ProtocolKind::GeminiNative | ProtocolKind::Copilot
        ) && self.base_url.is_some()
    }

    fn supports_device_flow(&self) -> bool {
        matches!(self.protocol, ProtocolKind::Copilot)
    }

    fn caps(&self) -> ProviderCaps {
        self.protocol_caps()
    }

    async fn list_models_api(&self) -> Result<Option<Vec<String>>> {
        let Some(base) = self.base_url.clone() else {
            return Ok(None);
        };
        match self.protocol {
            ProtocolKind::OpenAiCompat => {
                let key = self.key().await?;
                let headers = vec![("authorization".to_string(), format!("Bearer {key}"))];
                let body = http::get_json(
                    &self.http,
                    &format!("{}/models", base.trim_end_matches('/')),
                    headers,
                )
                .await?;
                Ok(Some(wire_openai::parse_models_list(&body)?))
            }
            ProtocolKind::GeminiNative => {
                let key = self.key().await?;
                let headers = vec![("x-goog-api-key".to_string(), key)];
                let body = http::get_json(
                    &self.http,
                    &format!("{}/models", base.trim_end_matches('/')),
                    headers,
                )
                .await?;
                Ok(Some(gemini::parse_models_list(&body)?))
            }
            ProtocolKind::Copilot => {
                let token = copilot_live_token(&self.auth_store).await?;
                let body = http::get_json(
                    &self.http,
                    &format!("{}/models", base.trim_end_matches('/')),
                    copilot_headers(&token),
                )
                .await?;
                let mut ids: Vec<String> = body
                    .pointer("/data")
                    .and_then(|d| d.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter(|m| is_chat_model(m))
                            .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                ids.sort();
                Ok(Some(ids))
            }
            _ => Ok(None),
        }
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse> {
        // Enforce protocol capabilities before building any wire payload:
        // never advertise tools/caching the route cannot carry.
        let mut warnings: Vec<String> = Vec::new();
        let effective = self.effective_request(request, &mut warnings);
        let request = &effective;
        let result = async {
            match self.protocol {
                ProtocolKind::OpenAiCompat => {
                    let base = self.base_url.clone().ok_or_else(|| {
                        CoreError::Other(format!(
                            "provider `{}` has no API URL — configure base_url in providers/*.toml",
                            self.id
                        ))
                    })?;
                    let key = self.key().await?;
                    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
                    let headers = vec![("authorization".to_string(), format!("Bearer {key}"))];
                    self.chat_openai_wire(url, headers, request).await
                }
                ProtocolKind::OpenAiCompatIbmIam => {
                    let api_key = self.key().await?;
                    let token = preauth::ibm_iam_token(&api_key).await?;
                    let host = std::env::var("WATSONX_AI_URL")
                        .unwrap_or_else(|_| WATSONX_DEFAULT_HOST.into());
                    let project = std::env::var("WATSONX_AI_PROJECT_ID").map_err(|_| {
                        CoreError::Other("watsonx: set WATSONX_AI_PROJECT_ID".into())
                    })?;
                    let url = format!(
                        "{}/ml/v1/text/chat?version={WATSONX_API_VERSION}&project_id={project}",
                        host.trim_end_matches('/')
                    );
                    let headers = vec![("authorization".to_string(), format!("Bearer {token}"))];
                    self.chat_openai_wire(url, headers, request).await
                }
                ProtocolKind::OpenAiCompatOauthCc => {
                    let (base, token) = self.sap_ai_core_endpoint().await?;
                    let url = format!("{base}/v2/chat/completions");
                    let headers = vec![("authorization".to_string(), format!("Bearer {token}"))];
                    self.chat_openai_wire(url, headers, request).await
                }
                ProtocolKind::AnthropicCompat => {
                    let base = self.base_url.clone().ok_or_else(|| missing_base(self))?;
                    let key = self.key().await?;
                    let url = format!("{}/messages", base.trim_end_matches('/'));
                    let headers = vec![
                        ("x-api-key".to_string(), key),
                        ("anthropic-version".to_string(), "2023-06-01".into()),
                    ];
                    let started = std::time::Instant::now();
                    let payload = crate::wire_anthropic::build_chat_payload(
                        request,
                        crate::wire_anthropic::AnthropicWireOpts {
                            supports_thinking: true,
                        },
                    );
                    let body = http::post_json(&self.http, &url, headers, &payload).await?;
                    crate::wire_anthropic::parse_chat_response(
                        &body,
                        started.elapsed().as_millis() as u64,
                        Vec::new(),
                    )
                }
                ProtocolKind::Azure => {
                    // The two catalog entries use different env names for the
                    // resource (and different default domains).
                    let resource_env = if self.id == "azure-cognitive-services" {
                        "AZURE_COGNITIVE_SERVICES_RESOURCE_NAME"
                    } else {
                        "AZURE_RESOURCE_NAME"
                    };
                    let base = azure::resolve_base(self.base_url.as_deref(), resource_env)?;
                    let key = self.key().await?;
                    azure::chat(&self.http, &base, &key, request).await
                }
                ProtocolKind::GeminiNative => {
                    let base = self.base_url.clone().ok_or_else(|| missing_base(self))?;
                    let key = self.key().await?;
                    gemini::chat(&self.http, &base, &key, request).await
                }
                ProtocolKind::VertexGemini => {
                    vertex::chat(&self.http, false, request, &self.auth_store).await
                }
                ProtocolKind::VertexAnthropic => {
                    vertex::chat(&self.http, true, request, &self.auth_store).await
                }
                ProtocolKind::Bedrock => bedrock::chat(&self.http, request, &self.auth_store).await,
                ProtocolKind::Cohere => {
                    let base = self.base_url.clone().ok_or_else(|| missing_base(self))?;
                    let key = self.key().await?;
                    cohere::chat(&self.http, &base, &key, request).await
                }
                ProtocolKind::GitLabDuo => {
                    let base = self
                        .base_url
                        .clone()
                        .or_else(|| std::env::var("GITLAB_AI_GATEWAY_URL").ok())
                        .ok_or_else(|| missing_base(self))?;
                    let key = self.key().await?;
                    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
                    let headers = vec![("authorization".to_string(), format!("Bearer {key}"))];
                    self.chat_openai_wire(url, headers, request).await
                }
                ProtocolKind::Copilot => {
                    let base = self
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.githubcopilot.com".into());
                    let token = copilot_live_token(&self.auth_store).await?;
                    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
                    self.chat_openai_wire(url, copilot_headers(&token), request)
                        .await
                }
            }
        }
        .await;
        result.map(|mut resp| {
            let mut all = warnings;
            all.append(&mut resp.warnings);
            resp.warnings = all;
            resp
        })
    }

    /// Streaming variant. Protocols without SSE support fall back to the
    /// trait default (single `Completed` item over the non-streaming path).
    async fn chat_stream(&self, request: &ChatRequest) -> Result<lmhub_core::ChatStream> {
        match self.protocol {
            ProtocolKind::OpenAiCompat
            | ProtocolKind::OpenAiCompatIbmIam
            | ProtocolKind::OpenAiCompatOauthCc
            | ProtocolKind::GitLabDuo
            | ProtocolKind::Copilot
            | ProtocolKind::Azure => {
                let (url, headers) = self.openai_stream_endpoint(request).await?;
                stream_runner::openai_sse(&self.http, url, headers, request).await
            }
            ProtocolKind::AnthropicCompat => {
                let base = self.base_url.clone().ok_or_else(|| missing_base(self))?;
                let key = self.key().await?;
                let url = format!("{}/messages", base.trim_end_matches('/'));
                let headers = vec![
                    ("x-api-key".to_string(), key),
                    ("anthropic-version".to_string(), "2023-06-01".into()),
                ];
                stream_runner::anthropic_sse(&self.http, url, headers, request).await
            }
            ProtocolKind::GeminiNative => {
                let base = self.base_url.clone().ok_or_else(|| missing_base(self))?;
                let key = self.key().await?;
                let url = gemini::stream_url(&base, &request.model);
                let headers = vec![("x-goog-api-key".to_string(), key)];
                stream_runner::gemini_sse(&self.http, url, headers, request).await
            }
            ProtocolKind::VertexGemini => {
                vertex::chat_stream(&self.http, false, request, &self.auth_store).await
            }
            ProtocolKind::VertexAnthropic => {
                vertex::chat_stream(&self.http, true, request, &self.auth_store).await
            }
            // Bedrock / Cohere: non-streaming fallback via trait default.
            _ => {
                let response = self.chat(request).await?;
                Ok(lmhub_core::ChatRequest::completed_stream(response))
            }
        }
    }
}

impl RoutedProvider {
    /// URL+headers for every OpenAI-wire protocol flavor (chat + streaming).
    async fn openai_stream_endpoint(
        &self,
        request: &ChatRequest,
    ) -> Result<(String, Vec<(String, String)>)> {
        match self.protocol {
            ProtocolKind::OpenAiCompat => {
                let base = self.base_url.clone().ok_or_else(|| missing_base(self))?;
                let key = self.key().await?;
                Ok((
                    format!("{}/chat/completions", base.trim_end_matches('/')),
                    vec![("authorization".to_string(), format!("Bearer {key}"))],
                ))
            }
            ProtocolKind::OpenAiCompatOauthCc => {
                // Same service-key + client-credentials auth as the
                // non-streaming path, so streaming actually works for SAP.
                let (base, token) = self.sap_ai_core_endpoint().await?;
                Ok((
                    format!("{base}/v2/chat/completions"),
                    vec![("authorization".to_string(), format!("Bearer {token}"))],
                ))
            }
            ProtocolKind::Azure => {
                let resource_env = if self.id == "azure-cognitive-services" {
                    "AZURE_COGNITIVE_SERVICES_RESOURCE_NAME"
                } else {
                    "AZURE_RESOURCE_NAME"
                };
                let base = azure::resolve_base(self.base_url.as_deref(), resource_env)?;
                let key = self.key().await?;
                Ok((
                    azure::stream_url(&base, &request.model),
                    vec![("api-key".to_string(), key)],
                ))
            }
            ProtocolKind::GitLabDuo => {
                let base = self
                    .base_url
                    .clone()
                    .or_else(|| std::env::var("GITLAB_AI_GATEWAY_URL").ok())
                    .ok_or_else(|| missing_base(self))?;
                let key = self.key().await?;
                Ok((
                    format!("{}/chat/completions", base.trim_end_matches('/')),
                    vec![("authorization".to_string(), format!("Bearer {key}"))],
                ))
            }
            ProtocolKind::Copilot => {
                let base = self
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.githubcopilot.com".into());
                let token = copilot_live_token(&self.auth_store).await?;
                Ok((
                    format!("{}/chat/completions", base.trim_end_matches('/')),
                    copilot_headers(&token),
                ))
            }
            ProtocolKind::OpenAiCompatIbmIam => {
                let api_key = self.key().await?;
                let token = preauth::ibm_iam_token(&api_key).await?;
                let host =
                    std::env::var("WATSONX_AI_URL").unwrap_or_else(|_| WATSONX_DEFAULT_HOST.into());
                let project = std::env::var("WATSONX_AI_PROJECT_ID")
                    .map_err(|_| CoreError::Other("watsonx: set WATSONX_AI_PROJECT_ID".into()))?;
                Ok((
                    format!(
                        "{}/ml/v1/text/chat?version={WATSONX_API_VERSION}&project_id={project}",
                        host.trim_end_matches('/')
                    ),
                    vec![("authorization".to_string(), format!("Bearer {token}"))],
                ))
            }
            _ => unreachable!("non-OpenAI protocols never call openai_stream_endpoint"),
        }
    }

    /// SAP AI Core endpoint resolution shared by chat and chat_stream:
    /// service-key JSON supplies the client credentials and base URL.
    /// A TOML `base_url` may substitute for the service key's `url`.
    async fn sap_ai_core_endpoint(&self) -> Result<(String, String)> {
        let raw = std::env::var("AICORE_SERVICE_KEY").map_err(|_| {
            CoreError::MissingApiKey(
                "sap-ai-core: set AICORE_SERVICE_KEY to the service-key JSON".into(),
            )
        })?;
        let sk: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| CoreError::Parse(format!("AICORE_SERVICE_KEY: {e}")))?;
        let client_id = sk
            .get("clientid")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let secret = sk
            .get("clientsecret")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let token_url = sk
            .get("token_service_url")
            .and_then(|v| v.as_str())
            .map(|u| u.trim_end_matches('/').to_string())
            .unwrap_or_else(|| "https://authentication.eu10.hana.ondemand.com/oauth/token".into());
        let token = preauth::oauth_client_credentials(client_id, secret, &token_url).await?;
        let base = self.base_url.clone().or_else(|| {
            sk.get("url")
                .and_then(|v| v.as_str())
                .map(|s| s.trim_end_matches('/').to_string())
        });
        let base = base.ok_or_else(|| {
            CoreError::Other(
                "sap-ai-core: service key has no `url`; configure base_url in TOML".into(),
            )
        })?;
        Ok((base, token))
    }
}

fn missing_base(p: &RoutedProvider) -> CoreError {
    CoreError::Other(format!(
        "provider `{}` needs an API URL (catalog entry incomplete — add base_url in providers/*.toml)",
        p.id
    ))
}

/// Copilot tokens expire fast; exchange lazily and refresh the stored blob.
async fn copilot_live_token(store_lock: &std::sync::Mutex<AuthStore>) -> Result<String> {
    {
        let store = store_lock.lock().unwrap();
        if let Some(cred) = store.get(copilot::PROVIDER_ID) {
            let now = chrono::Utc::now().timestamp();
            let live = cred.expires_at.map(|exp| exp > now).unwrap_or(false);
            if let (Some(access), true) = (cred.access_token.clone(), live) {
                return Ok(access); // still valid — use as-is
            }
        }
    }
    // Expired or missing: re-exchange from the stored GitHub token.
    let github_token = {
        let store = store_lock.lock().unwrap();
        store.get(copilot::PROVIDER_ID).and_then(|c| c.key.clone())
    };
    let Some(github_token) = github_token else {
        return Err(CoreError::MissingApiKey(
            "github-copilot: select it in Setup and press Enter to complete the device flow".into(),
        ));
    };
    let (token, expires_at) = copilot::copilot_token(&github_token).await?;
    let mut store = store_lock.lock().unwrap();
    store.set_credential(
        copilot::PROVIDER_ID,
        lmhub_core::StoredCredential {
            kind: "oauth".into(),
            key: Some(github_token),
            access_token: Some(token.clone()),
            expires_at: Some(expires_at),
            refresh_token: None,
        },
    );
    // Persist refreshed tokens so they survive restarts.
    store.save().map_err(CoreError::Io)?;
    Ok(token)
}

fn copilot_headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("authorization".to_string(), format!("Bearer {token}")),
        ("editor-version".to_string(), copilot::editor_version()),
        (
            "editor-plugin-version".to_string(),
            copilot::EDITOR_PLUGIN_VERSION.into(),
        ),
        (
            "copilot-integration-id".to_string(),
            copilot::INTEGRATION_ID.into(),
        ),
        // Programmatic agent traffic (not user-initiated typing).
        ("x-initiator".to_string(), "agent".into()),
        ("user-agent".to_string(), "lmhub-runner".into()),
    ]
}

fn is_chat_model(m: &serde_json::Value) -> bool {
    m.pointer("/capabilities/type")
        .and_then(|t| t.as_str())
        .map(|t| t == "chat")
        .unwrap_or(true)
}

impl RoutedProvider {
    /// Request as the route will actually carry it: tools stripped when the
    /// protocol cannot use them, prompt caching disabled when unsupported.
    fn effective_request(&self, request: &ChatRequest, warnings: &mut Vec<String>) -> ChatRequest {
        let caps = self.protocol_caps();
        let mut effective = request.clone();
        if !caps.prompt_caching {
            effective.enable_prompt_cache = false;
        }
        if !caps.tool_calls && !effective.tools.is_empty() {
            effective.tools.clear();
            warnings.push(format!(
                "provider `{}` protocol does not support tools; tools stripped",
                self.id
            ));
        }
        effective
    }

    /// Per-protocol capability truth (AGENTS invariant 2: adapters must not
    /// over-claim). Per-model flags from Models.dev narrow this further at
    /// selection time; unsupported optional params still degrade via adapter
    /// retries, but we never *advertise* a feature the wire cannot carry.
    fn protocol_caps(&self) -> ProviderCaps {
        match self.protocol {
            // Converse: tools yes; thinking is not sent by our adapter and
            // prompt caching (cachePoint) is not implemented.
            ProtocolKind::Bedrock => ProviderCaps {
                tool_calls: true,
                reasoning: false,
                prompt_caching: false,
            },
            // Cohere v2: tools yes; reasoning config is not part of the wire.
            ProtocolKind::Cohere => ProviderCaps {
                tool_calls: true,
                reasoning: false,
                prompt_caching: false,
            },
            // Gemini: thinkingConfig is sent; implicit caching is server-side
            // but nothing in our payload controls it.
            ProtocolKind::GeminiNative | ProtocolKind::VertexGemini => ProviderCaps {
                tool_calls: true,
                reasoning: true,
                prompt_caching: false,
            },
            // OpenAI/Anthropic wires carry all three when the model supports
            // them; the model's own flags decide at selection time.
            _ => ProviderCaps {
                tool_calls: true,
                reasoning: true,
                prompt_caching: true,
            },
        }
    }
}
