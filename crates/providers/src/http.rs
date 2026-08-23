//! Shared HTTP plumbing for provider adapters.
//!
//! API keys are read from the environment **at call time** and placed only
//! into request headers. They are never logged, persisted or embedded in
//! error messages.
//!
//! Every request goes through [`send_request`], which applies the configured
//! retry policy to transient conditions (429/5xx/transport errors) with
//! exponential backoff + jitter and `Retry-After` precedence. Streaming
//! callers reuse the same helper — retries happen strictly **before** the
//! first body byte, never mid-stream.

use lmhub_core::{redact, CoreError, Result};
use reqwest::Client;
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const GET_TIMEOUT: Duration = Duration::from_secs(60);

/// Backoff policy (loaded from AppConfig at startup).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts per request, including the first one.
    pub max_attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 6,
            base: Duration::from_millis(500),
            cap: Duration::from_secs(30),
        }
    }
}

static POLICY: OnceLock<RetryPolicy> = OnceLock::new();

/// Install the process-wide policy (called from main with AppConfig).
/// `max_attempts` is clamped to >= 1 so the retry loop always runs.
pub fn init_retry_policy(policy: RetryPolicy) {
    let policy = RetryPolicy {
        max_attempts: policy.max_attempts.max(1),
        ..policy
    };
    if POLICY.set(policy).is_err() {
        tracing::warn!("init_retry_policy called twice; second policy ignored");
    }
}

fn policy() -> RetryPolicy {
    *POLICY.get().unwrap_or(&RetryPolicy::default())
}

fn retryable_status(code: u16) -> bool {
    matches!(code, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Pure decision: how long to wait before retry number `attempt`
/// (0-based index of the FAILED attempt), or `None` when exhausted/fatal.
///
/// `Retry-After` wins over backoff and bypasses the cap (hard-limited to
/// 10 minutes); otherwise exponential `base * 2^attempt`, clamped to `cap`.
pub(crate) fn next_delay(
    attempt: u32,
    status: Option<u16>,
    retry_after_secs: Option<u64>,
    policy: &RetryPolicy,
) -> Option<Duration> {
    if let Some(code) = status {
        if !retryable_status(code) {
            return None;
        }
    }
    if attempt + 1 >= policy.max_attempts {
        return None;
    }
    if let Some(secs) = retry_after_secs {
        return Some(Duration::from_secs(secs.min(600)));
    }
    let exp = policy.base.saturating_mul(2u32.saturating_pow(attempt));
    Some(exp.min(policy.cap))
}

/// Add ±15% jitter to a computed delay (cheap time-seeded PRNG; not security
/// relevant — only spreads thundering herds).
fn jitter(delay: Duration) -> Duration {
    let millis = delay.as_millis() as u64;
    if millis == 0 {
        return delay;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as u64)
        .unwrap_or(0);
    let spread = millis / 7; // ~±14%
    let offset = nanos % (spread * 2 + 1);
    Duration::from_millis(millis - spread + offset)
}

async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// Send one HTTP request with retry-on-transient semantics.
/// Returns the final `Response` (success OR exhausted non-retryable error —
/// callers turn those into their domain errors exactly as before).
pub(crate) async fn send_request(
    http: &Client,
    method: reqwest::Method,
    url: &str,
    headers: Vec<(String, String)>,
    body: Option<String>,
    timeout: Duration,
) -> Result<reqwest::Response> {
    let policy = policy();
    let header_map = to_header_map(headers)?;

    for attempt in 0..policy.max_attempts {
        let mut req = http.request(method.clone(), url).timeout(timeout);
        req = req.headers(header_map.clone());
        if let Some(body) = &body {
            req = req.body(body.clone());
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() || !retryable_status(status.as_u16()) {
                    return Ok(resp);
                }
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after);
                if let Some(delay) =
                    next_delay(attempt, Some(status.as_u16()), retry_after, &policy)
                {
                    tracing::warn!(
                        url = %redact::scrub(url),
                        status = status.as_u16(),
                        attempt = attempt + 1,
                        sleep_ms = delay.as_millis() as u64,
                        "transient provider error; retrying"
                    );
                    sleep(jitter(delay)).await;
                    continue;
                }
                // Retries exhausted on a retryable status: classify as
                // Transient (with the upstream's body) so callers can tell a
                // 429-exhausted request from a hard 4xx.
                tracing::warn!(
                    url = %redact::scrub(url),
                    status = status.as_u16(),
                    attempts = attempt + 1,
                    "giving up after retries"
                );
                let text = resp.text().await.unwrap_or_default();
                return Err(CoreError::Transient {
                    code: Some(status.as_u16()),
                    retry_after_secs: retry_after,
                    message: format!(
                        "HTTP {}: {} (after {} attempts)",
                        status.as_u16(),
                        truncate_body(&text),
                        attempt + 1
                    ),
                });
            }
            Err(e) => {
                let transport_transient =
                    e.is_connect() || e.is_timeout() || e.is_request() || e.is_body();
                if transport_transient && attempt + 1 < policy.max_attempts {
                    let delay = next_delay(attempt, None, None, &policy).unwrap_or(policy.cap);
                    tracing::warn!(
                        url = %redact::scrub(url),
                        attempt = attempt + 1,
                        error = %redact::scrub(&e.to_string()),
                        "transport error; retrying"
                    );
                    sleep(jitter(delay)).await;
                    continue;
                }
                return Err(CoreError::Http(scrub_err(&e)));
            }
        }
    }
    unreachable!("retry loop always returns within max_attempts")
}

/// Parse `Retry-After`: delta-seconds or an HTTP-date.
fn parse_retry_after(raw: &str) -> Option<u64> {
    if let Ok(secs) = raw.trim().parse::<u64>() {
        return Some(secs);
    }
    chrono::DateTime::parse_from_rfc2822(raw).ok().map(|dt| {
        dt.with_timezone(&chrono::Utc)
            .signed_duration_since(chrono::Utc::now())
            .max(chrono::Duration::zero())
            .num_seconds() as u64
    })
}

/// POST a JSON body with auth headers; parse the JSON response.
pub(crate) async fn post_json(
    http: &Client,
    url: &str,
    headers: Vec<(String, String)>,
    body: &Value,
) -> Result<Value> {
    let payload = serde_json::to_string(body)
        .map_err(|e| CoreError::Other(format!("serialize request: {e}")))?;
    let resp = send_request(
        http,
        reqwest::Method::POST,
        url,
        headers,
        Some(payload),
        REQUEST_TIMEOUT,
    )
    .await?;
    take_json(resp).await
}

/// GET JSON with auth headers.
pub(crate) async fn get_json(
    http: &Client,
    url: &str,
    headers: Vec<(String, String)>,
) -> Result<Value> {
    let resp = send_request(http, reqwest::Method::GET, url, headers, None, GET_TIMEOUT).await?;
    take_json(resp).await
}

/// POST expecting a streaming (SSE/binary) response body.
/// Retries cover connection establishment + status; the body must be read
/// by the caller (retries never apply once streaming started).
pub(crate) async fn post_stream(
    http: &Client,
    url: &str,
    headers: Vec<(String, String)>,
    body: String,
) -> Result<reqwest::Response> {
    send_request(
        http,
        reqwest::Method::POST,
        url,
        headers,
        Some(body),
        REQUEST_TIMEOUT,
    )
    .await
}

async fn take_json(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| CoreError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(CoreError::Provider(format!(
            "HTTP {}: {}",
            status.as_u16(),
            truncate_body(&text)
        )));
    }
    serde_json::from_str(&text)
        .map_err(|e| CoreError::Parse(format!("{e}; body: {}", truncate_body(&text))))
}

fn truncate_body(body: &str) -> String {
    let scrubbed = redact::scrub(body);
    let mut s: String = scrubbed.chars().take(600).collect();
    if scrubbed.chars().count() > 600 {
        s.push('…');
    }
    s
}

fn to_header_map(headers: Vec<(String, String)>) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| CoreError::Other(format!("bad header name {name:?}: {e}")))?;
        let value = HeaderValue::from_str(&value)
            .map_err(|_| CoreError::Other("invalid header value".to_string()))?;
        map.insert(name, value);
    }
    Ok(map)
}

fn scrub_err(e: &reqwest::Error) -> String {
    // reqwest errors can embed request URLs but never our headers; still scrub defensively.
    redact::scrub(&e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: RetryPolicy = RetryPolicy {
        max_attempts: 6,
        base: Duration::from_millis(500),
        cap: Duration::from_secs(30),
    };

    #[test]
    fn exponential_backoff_with_cap() {
        assert_eq!(
            next_delay(0, Some(500), None, &P),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            next_delay(1, Some(500), None, &P),
            Some(Duration::from_millis(1_000))
        );
        assert_eq!(
            next_delay(3, Some(502), None, &P),
            Some(Duration::from_millis(4_000))
        );
        assert_eq!(next_delay(4, None, None, &P), Some(Duration::from_secs(8)));
        // Attempt 5 is the last of 6 — nothing follows it.
        assert_eq!(next_delay(5, Some(429), None, &P), None);
    }

    #[test]
    fn exhausts_after_max_attempts() {
        assert_eq!(next_delay(6, Some(429), None, &P), None);
        let tighter = RetryPolicy {
            max_attempts: 3,
            ..P
        };
        assert_eq!(next_delay(2, Some(503), None, &tighter), None);
        assert!(next_delay(1, Some(503), None, &tighter).is_some());
    }

    #[test]
    fn retry_after_wins_and_hard_caps_at_ten_minutes() {
        assert_eq!(
            next_delay(0, Some(429), Some(3), &P),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            next_delay(0, Some(429), Some(90_000), &P),
            Some(Duration::from_secs(600))
        );
    }

    #[test]
    fn non_retryable_statuses_are_fatal() {
        assert_eq!(next_delay(0, Some(400), None, &P), None);
        assert_eq!(next_delay(0, Some(401), None, &P), None);
        assert_eq!(next_delay(0, Some(404), None, &P), None);
    }

    #[test]
    fn parses_retry_after_formats() {
        assert_eq!(parse_retry_after("12"), Some(12));
        let future = (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc2822();
        let got = parse_retry_after(&future).unwrap();
        assert!((25..=35).contains(&got), "{got}");
        assert_eq!(parse_retry_after("garbage"), None);
    }
}
