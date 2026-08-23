//! GitHub Copilot: device-flow OAuth + Copilot API bearer exchange.
//!
//! Flow (mirrors opencode):
//! 1. `POST github.com/login/device/code` → user_code + verification_uri
//!    (user approves in browser);
//! 2. poll `login/oauth/access_token` until authorized;
//! 3. exchange the GitHub token for a short-lived **Copilot token** at
//!    `api.github.com/copilot_internal/v2/token` — cached in auth.json;
//! 4. chat requests hit `https://api.githubcopilot.com` (OpenAI wire) with
//!    the Copilot token plus integration headers.

use lmhub_core::{AuthStore, CoreError, Result, StoredCredential};
use serde_json::Value;
use std::time::Duration;

pub const DEFAULT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98"; // opencode's public client id
pub const PROVIDER_ID: &str = "github-copilot";

fn client_id() -> String {
    std::env::var("LMHUB_GITHUB_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceFlowEvent {
    /// Show this code to the user; keep polling.
    Awaiting {
        user_code: String,
        verification_uri: String,
        /// Opaque code needed for polling.
        device_code: String,
    },
    /// User authorized — GitHub access token obtained.
    Authorized { github_token: String },
    /// Device code expired before authorization.
    Expired,
    /// GitHub asked the client to slow down; keep polling at a longer interval.
    SlowDown,
    /// Unrecoverable error from GitHub.
    Failed(String),
}

/// Pure state machine over a single poll response — trivially testable.
pub fn interpret_poll_response(status: u16, body: &Value) -> Option<DeviceFlowEvent> {
    match status {
        200 => Some(DeviceFlowEvent::Authorized {
            github_token: body
                .get("access_token")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        }),
        400 | 428 => {
            let err = body.get("error").and_then(|v| v.as_str()).unwrap_or("");
            match err {
                "authorization_pending" => None, // keep polling
                "slow_down" => Some(DeviceFlowEvent::SlowDown),
                "expired_token" => Some(DeviceFlowEvent::Expired),
                _ => Some(DeviceFlowEvent::Failed(err.to_string())),
            }
        }
        _ => None,
    }
}

/// Step 1: start the device flow. Returns what to show the user + device code.
pub async fn start_device_flow() -> Result<DeviceFlowEvent> {
    let resp = reqwest::Client::new()
        .post("https://github.com/login/device/code")
        .header("accept", "application/json")
        .form(&[("client_id", client_id().as_str()), ("scope", "read:user")])
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| CoreError::Http(e.to_string()))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| CoreError::Parse(e.to_string()))?;
    if !status.is_success() {
        return Err(CoreError::Provider(format!(
            "copilot device-code start failed: HTTP {} {}",
            status.as_u16(),
            body.get("error_description")
                .and_then(|v| v.as_str())
                .or_else(|| body.get("error").and_then(|v| v.as_str()))
                .unwrap_or("unknown error")
        )));
    }
    Ok(DeviceFlowEvent::Awaiting {
        user_code: body
            .get("user_code")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .into(),
        verification_uri: body
            .get("verification_uri")
            .and_then(|v| v.as_str())
            .unwrap_or("https://github.com/login/device")
            .into(),
        device_code: body
            .get("device_code")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .into(),
    })
}

/// Full interactive flow: start → show code → poll → exchange → store.
/// `notify` receives human-readable progress lines for the TUI.
pub async fn run_full_flow(
    store: &std::sync::Arc<std::sync::Mutex<AuthStore>>,
    notify: impl Fn(String),
) -> Result<()> {
    let started = start_device_flow().await?;
    let DeviceFlowEvent::Awaiting {
        user_code,
        verification_uri,
        device_code,
    } = started
    else {
        return Err(CoreError::Other("unexpected device-flow state".into()));
    };
    notify(format!(
        "copilot: open {verification_uri} and enter code {user_code} (waiting up to 15 min)"
    ));
    let mut slow_down = false;
    let mut consecutive_errors = 0u32;
    for _ in 0..180 {
        // GitHub's spec: poll every 5s, every 10s after a slow_down.
        tokio::time::sleep(if slow_down {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(5)
        })
        .await;
        slow_down = false;
        // Transient network errors must not kill the flow — but a run of
        // failures that will never recover should not burn 15 minutes.
        let event = match poll_device_flow(&device_code).await {
            Ok(Some(ev)) => ev,
            Ok(None) => {
                consecutive_errors = 0;
                continue;
            }
            Err(_) => {
                consecutive_errors += 1;
                if consecutive_errors >= 5 {
                    return Err(CoreError::Http(
                        "copilot device flow: 5 consecutive poll failures; giving up".into(),
                    ));
                }
                continue;
            }
        };
        consecutive_errors = 0;
        return match event {
            DeviceFlowEvent::Authorized { github_token } => {
                let (token, expires_at) = copilot_token(&github_token).await?;
                let mut store = store.lock().unwrap();
                store.set_credential(
                    PROVIDER_ID,
                    StoredCredential {
                        kind: "oauth".into(),
                        key: Some(github_token),
                        access_token: Some(token),
                        expires_at: Some(expires_at),
                        refresh_token: None,
                    },
                );
                // Persist so the token survives restarts; without this the
                // user would redo the whole device flow every launch.
                store.save()?;
                notify("copilot: connected ✔".into());
                Ok(())
            }
            DeviceFlowEvent::SlowDown => {
                slow_down = true;
                continue;
            }
            DeviceFlowEvent::Expired => Err(CoreError::Other(
                "device code expired — restart the connect flow".into(),
            )),
            DeviceFlowEvent::Failed(msg) => Err(CoreError::Provider(format!("copilot: {msg}"))),
            DeviceFlowEvent::Awaiting { .. } => {
                continue; // unreachable from polls; keep looping defensively
            }
        };
    }
    Err(CoreError::Timeout)
}

/// Poll once with a device code. Caller loops respecting the interval.
pub async fn poll_device_flow(device_code: &str) -> Result<Option<DeviceFlowEvent>> {
    let resp = reqwest::Client::new()
        .post("https://github.com/login/oauth/access_token")
        .header("accept", "application/json")
        .form(&[
            ("client_id", client_id().as_str()),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| CoreError::Http(e.to_string()))?;
    let status = resp.status().as_u16();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| CoreError::Parse(e.to_string()))?;
    Ok(interpret_poll_response(status, &body))
}

/// Step 3: GitHub token → short-lived Copilot API token.
pub async fn copilot_token(github_token: &str) -> Result<(String, i64)> {
    let resp = reqwest::Client::new()
        .get("https://api.github.com/copilot_internal/v2/token")
        .bearer_auth(github_token)
        .header("user-agent", "lmhub-runner")
        .header("editor-version", "vscode/1.95.0")
        .header("editor-plugin-version", "copilot/1.0")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| CoreError::Http(lmhub_core::redact::scrub(&e.to_string())))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| CoreError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(CoreError::Provider(format!(
            "copilot token exchange failed: HTTP {} {}",
            status.as_u16(),
            lmhub_core::redact::scrub(&text.chars().take(300).collect::<String>())
        )));
    }
    let parsed: Value = serde_json::from_str(&text).map_err(|e| CoreError::Parse(e.to_string()))?;
    let token = parsed
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Parse("copilot token response missing `token`".into()))?
        .to_string();
    // expires_at is unix seconds in the Copilot response.
    let expires_at = parsed
        .get("expires_at")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| chrono::Utc::now().timestamp() + 1_500);
    Ok((token, expires_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn poll_state_machine() {
        assert_eq!(
            interpret_poll_response(400, &json!({"error": "authorization_pending"})),
            None,
            "pending keeps polling"
        );
        assert_eq!(
            interpret_poll_response(428, &json!({"error": "slow_down"})),
            Some(DeviceFlowEvent::SlowDown)
        );
        assert_eq!(
            interpret_poll_response(400, &json!({"error": "expired_token"})),
            Some(DeviceFlowEvent::Expired)
        );
        assert_eq!(
            interpret_poll_response(
                200,
                &json!({"access_token": "gho_x", "token_type": "bearer"})
            ),
            Some(DeviceFlowEvent::Authorized {
                github_token: "gho_x".into()
            })
        );
    }
}
