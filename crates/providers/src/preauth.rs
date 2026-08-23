//! Pre-auth hooks for OpenAI-compatible endpoints that need a dynamic
//! bearer token obtained from another auth service:
//! - `IbmIam`: IBM Cloud IAM apikey → access token (watsonx);
//! - `OauthClientCredentials`: client-credentials flow (SAP AI Core).

use lmhub_core::{CoreError, Result};
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct CachedToken {
    token: String,
    valid_until: Instant,
}

fn cached() -> &'static Mutex<Option<(String, CachedToken)>> {
    static CACHE: std::sync::OnceLock<Mutex<Option<(String, CachedToken)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

async fn fetch_token(form: &[(&str, &str)], url: &str) -> Result<(String, u64)> {
    let resp = reqwest::Client::new()
        .post(url)
        .form(form)
        .timeout(Duration::from_secs(30))
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
            "pre-auth failed: HTTP {} {}",
            status.as_u16(),
            lmhub_core::redact::scrub(&text.chars().take(300).collect::<String>())
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
        .and_then(|v| v.as_u64())
        .unwrap_or(3_600);
    Ok((token, expires_in))
}

/// IBM IAM (watsonx): apikey → bearer. Cache keyed by apikey hash prefix.
pub async fn ibm_iam_token(api_key: &str) -> Result<String> {
    if let Some((key, c)) = cached().lock().unwrap().as_ref() {
        if key == "ibm-iam" && c.valid_until > Instant::now() {
            return Ok(c.token.clone());
        }
    }
    let (token, expires_in) = fetch_token(
        &[
            ("grant_type", "urn:ibm:params:oauth:grant-type:apikey"),
            ("apikey", api_key),
        ],
        "https://iam.cloud.ibm.com/identity/token",
    )
    .await?;
    *cached().lock().unwrap() = Some((
        "ibm-iam".to_string(),
        CachedToken {
            token: token.clone(),
            valid_until: Instant::now()
                + Duration::from_secs(expires_in.saturating_sub(120).max(60)),
        },
    ));
    Ok(token)
}

/// Generic OAuth2 client-credentials against a fixed token URL.
pub async fn oauth_client_credentials(
    client_id: &str,
    client_secret: &str,
    token_url: &str,
) -> Result<String> {
    let cache_key = format!("cc:{client_id}:{token_url}");
    if let Some((key, c)) = cached().lock().unwrap().as_ref() {
        if *key == cache_key && c.valid_until > Instant::now() {
            return Ok(c.token.clone());
        }
    }
    let (token, expires_in) = fetch_token(
        &[
            ("grant_type", "client_credentials"),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ],
        token_url,
    )
    .await?;
    *cached().lock().unwrap() = Some((
        cache_key,
        CachedToken {
            token: token.clone(),
            valid_until: Instant::now()
                + Duration::from_secs(expires_in.saturating_sub(120).max(60)),
        },
    ));
    Ok(token)
}
