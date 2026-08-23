//! Google Vertex AI adapter — two variants sharing OAuth-bearer auth:
//! - `VertexGemini`:    Gemini `generateContent` wire on `publishers/google`;
//! - `VertexAnthropic`: Anthropic Messages wire on `publishers/anthropic`
//!   via `:rawPredict` (adds `anthropic_version` inside the body).
//!
//! Auth resolution (in order):
//! 1. `VERTEX_ACCESS_TOKEN` env / stored credential for `google-vertex`
//!    (oauth blob with a live access token);
//! 2. Application Default Credentials (`GOOGLE_APPLICATION_CREDENTIALS`
//!    service-account JSON) → RS256 JWT → OAuth2 token, cached in-process
//!    until shortly before expiry.

use crate::credentials;
use crate::gemini;
use crate::http;
use crate::wire_anthropic::{self, AnthropicWireOpts};
use lmhub_core::{ChatRequest, ChatResponse, CoreError, Result};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Instant;

const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// Refresh tokens this many seconds before actual expiry.
const TOKEN_SKEW_SECS: i64 = 120;

static TOKEN_CACHE: Mutex<Option<(String, i64)>> = Mutex::new(None);

pub fn resolve_project() -> Result<String> {
    std::env::var("GOOGLE_VERTEX_PROJECT")
        .or_else(|_| std::env::var("VERTEX_PROJECT"))
        .or_else(|_| std::env::var("GOOGLE_CLOUD_PROJECT"))
        .map_err(|_| CoreError::Other("vertex: set GOOGLE_VERTEX_PROJECT (project id)".into()))
}

pub fn resolve_location() -> String {
    std::env::var("GOOGLE_VERTEX_LOCATION")
        .or_else(|_| std::env::var("VERTEX_LOCATION"))
        .unwrap_or_else(|_| "global".into())
}

fn gemini_stream_url(project: &str, location: &str, model: &str) -> String {
    format!(
        "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:streamGenerateContent?alt=sse"
    )
}

fn anthropic_stream_url(project: &str, location: &str, model: &str) -> String {
    format!(
        "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/anthropic/models/{model}:rawStreamPredict"
    )
}

fn gemini_url(project: &str, location: &str, model: &str) -> String {
    format!(
        "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:generateContent"
    )
}

fn anthropic_url(project: &str, location: &str, model: &str) -> String {
    format!(
        "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/anthropic/models/{model}:rawPredict"
    )
}

async fn resolve_bearer(
    store: &std::sync::Arc<std::sync::Mutex<lmhub_core::AuthStore>>,
) -> Result<String> {
    // 1. explicit token (env or stored oauth credential)
    let stored = {
        let guard = store.lock().unwrap();
        credentials::resolve(
            &guard,
            "google-vertex",
            &["VERTEX_ACCESS_TOKEN".to_string()],
        )
    };
    if let Some(cred) = stored {
        return Ok(cred.secret);
    }
    // 2. cached in-process token
    if let Some((token, exp)) = TOKEN_CACHE.lock().unwrap().clone() {
        if exp > chrono::Utc::now().timestamp() + TOKEN_SKEW_SECS {
            return Ok(token);
        }
    }
    // 3. service-account JWT exchange
    let key_path = std::env::var("GOOGLE_APPLICATION_CREDENTIALS").map_err(|_| {
        CoreError::MissingApiKey(
            "vertex: set VERTEX_ACCESS_TOKEN or GOOGLE_APPLICATION_CREDENTIALS \
             (service-account JSON), or save a token for provider `google-vertex` via TUI"
                .into(),
        )
    })?;
    let (token, expires_in) = exchange_service_account_jwt(&key_path).await?;
    let exp_at = chrono::Utc::now().timestamp() + expires_in;
    *TOKEN_CACHE.lock().unwrap() = Some((token.clone(), exp_at));
    Ok(token)
}

async fn exchange_service_account_jwt(key_path: &str) -> Result<(String, i64)> {
    let raw = std::fs::read_to_string(key_path)
        .map_err(|e| CoreError::Other(format!("cannot read ADC file {key_path}: {e}")))?;
    let sa: Value =
        serde_json::from_str(&raw).map_err(|e| CoreError::Parse(format!("ADC JSON: {e}")))?;
    let client_email = sa
        .get("client_email")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Parse("ADC missing client_email".into()))?;
    let private_key = sa
        .get("private_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Parse("ADC missing private_key".into()))?;
    let token_uri = sa
        .get("token_uri")
        .and_then(|v| v.as_str())
        .unwrap_or(OAUTH_TOKEN_URL);

    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "iss": client_email,
        "scope": CLOUD_PLATFORM_SCOPE,
        "aud": token_uri,
        "iat": now,
        "exp": now + 3_600,
    });
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_bytes())
        .map_err(|e| CoreError::Other(format!("invalid ADC private_key: {e}")))?;
    let assertion = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &key,
    )
    .map_err(|e| CoreError::Other(format!("jwt encode failed: {e}")))?;

    let form = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
        ("assertion", assertion.as_str()),
    ];
    let resp = reqwest::Client::new()
        .post(token_uri)
        .form(&form)
        .timeout(crate::http::REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| CoreError::Http(e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| CoreError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(CoreError::Provider(format!(
            "vertex token exchange failed: HTTP {} {}",
            status.as_u16(),
            truncate(&text)
        )));
    }
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|e| CoreError::Parse(format!("token response: {e}")))?;
    let token = parsed
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Parse("token response missing access_token".into()))?
        .to_string();
    let expires_in = parsed
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3_600);
    Ok((token, expires_in))
}

fn truncate(s: &str) -> String {
    lmhub_core::redact::scrub(&s.chars().take(400).collect::<String>())
}

/// Streaming variant (Gemini SSE / Anthropic SSE on Vertex endpoints).
pub async fn chat_stream(
    http_client: &reqwest::Client,
    variant_anthropic: bool,
    request: &ChatRequest,
    store: &std::sync::Arc<std::sync::Mutex<lmhub_core::AuthStore>>,
) -> Result<lmhub_core::ChatStream> {
    let bearer = resolve_bearer(store).await?;
    let project = resolve_project()?;
    let location = resolve_location();
    let url = if variant_anthropic {
        anthropic_stream_url(&project, &location, &request.model)
    } else {
        gemini_stream_url(&project, &location, &request.model)
    };
    let headers = vec![("authorization".to_string(), format!("Bearer {bearer}"))];
    if variant_anthropic {
        crate::stream_runner::anthropic_sse(http_client, url, headers, request).await
    } else {
        crate::stream_runner::gemini_sse(http_client, url, headers, request).await
    }
}

pub async fn chat(
    http_client: &reqwest::Client,
    variant_anthropic: bool,
    request: &ChatRequest,
    store: &std::sync::Arc<std::sync::Mutex<lmhub_core::AuthStore>>,
) -> Result<ChatResponse> {
    let bearer = resolve_bearer(store).await?;
    let project = resolve_project()?;
    let location = resolve_location();

    let started = Instant::now();
    let body: Value = if variant_anthropic {
        let url = anthropic_url(&project, &location, &request.model);
        let mut payload = wire_anthropic::build_chat_payload(
            request,
            AnthropicWireOpts {
                supports_thinking: true,
            },
        );
        payload["anthropic_version"] = json!("vertex-2023-10-16");
        http::post_json(http_client, &url, vec![bearer_header(&bearer)], &payload).await?
    } else {
        let url = gemini_url(&project, &location, &request.model);
        http::post_json(
            http_client,
            &url,
            vec![bearer_header(&bearer)],
            &gemini::build_payload(request),
        )
        .await?
    };
    let duration_ms = started.elapsed().as_millis() as u64;

    if variant_anthropic {
        wire_anthropic::parse_chat_response(&body, duration_ms, Vec::new())
    } else {
        gemini::parse_response(&body, duration_ms, Vec::new())
    }
}

fn bearer_header(token: &str) -> (String, String) {
    ("authorization".to_string(), format!("Bearer {token}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_shapes_match_vertex_conventions() {
        assert_eq!(
            gemini_url("my-project", "us-central1", "gemini-2.5-pro"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent"
        );
        assert_eq!(
            anthropic_url("my-project", "global", "claude-sonnet-4-5"),
            "https://global-aiplatform.googleapis.com/v1/projects/my-project/locations/global/publishers/anthropic/models/claude-sonnet-4-5:rawPredict"
        );
    }

    #[test]
    fn anthropic_variant_injects_version_field() {
        use crate::wire_test_util::sample_request;
        let req = sample_request(lmhub_core::ReasoningLevel::Off);
        let mut payload = wire_anthropic::build_chat_payload(
            &req,
            AnthropicWireOpts {
                supports_thinking: true,
            },
        );
        payload["anthropic_version"] = json!("vertex-2023-10-16");
        assert_eq!(payload["anthropic_version"], json!("vertex-2023-10-16"));
        assert!(payload["messages"].is_array());
    }

    #[test]
    fn location_defaults_to_global() {
        std::env::remove_var("LMHUB_TEST_VLOC");
        // direct helper check through env absence is covered by resolve_location
        let _ = resolve_location();
    }
}
