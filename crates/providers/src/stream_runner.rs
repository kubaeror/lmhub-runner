//! Protocol-level SSE runners: request → byte stream → parsed deltas +
//! final [`ChatResponse`]. Shared by native adapters and `RoutedProvider`.
//!
//! Retry semantics live in [`http::post_stream`]: retries cover connection
//! establishment and non-success statuses only — never mid-stream bytes.

use crate::http;
use crate::sse::{self, SseEvent};
use crate::wire_anthropic::AnthropicStreamAccumulator;
use crate::wire_openai::{self, OpenAiWireOpts};
use crate::{gemini, wire_anthropic};
use lmhub_core::{
    ChatRequest, ChatResponse, ChatStream, ChatStreamItem, CoreError, ReasoningLevel, Result,
};
use std::time::Instant;

fn truncate_body(body: &str) -> String {
    lmhub_core::redact::scrub(&body.chars().take(600).collect::<String>())
}

async fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().await.unwrap_or_default();
    Err(CoreError::Provider(format!(
        "HTTP {}: {}",
        status.as_u16(),
        truncate_body(&text)
    )))
}

/// Copy of the request with reasoning disabled (degradation path when a
/// server rejects `reasoning_effort`).
fn without_reasoning(request: &ChatRequest) -> ChatRequest {
    let mut stripped = request.clone();
    stripped.reasoning = ReasoningLevel::Off;
    stripped
}

/// OpenAI-compatible streaming chat (`chat/completions`, SSE).
/// Sends `stream_options.include_usage`; degrades once without it (and once
/// without `reasoning_effort`) when the server rejects those fields.
pub(crate) async fn openai_sse(
    http_client: &reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    request: &ChatRequest,
) -> Result<ChatStream> {
    let mut payload = wire_openai::build_stream_payload(
        request,
        OpenAiWireOpts {
            include_reasoning_effort: true,
        },
    );

    let body_str = serde_json::to_string(&payload).expect("payload serializes");
    let first = http::post_stream(http_client, &url, headers.clone(), body_str).await;

    let resp = match first {
        Ok(r) => r,
        Err(CoreError::Provider(msg)) => {
            let lower = msg.to_ascii_lowercase();
            let strip_usage = lower.contains("stream_options");
            let strip_reasoning =
                lower.contains("reasoning_effort") && request.reasoning != ReasoningLevel::Off;
            if !(strip_usage || strip_reasoning) {
                return Err(CoreError::Provider(msg));
            }
            if strip_usage {
                wire_openai::strip_stream_options(&mut payload);
            }
            if strip_reasoning {
                payload = wire_openai::build_stream_payload(
                    &without_reasoning(request),
                    OpenAiWireOpts {
                        include_reasoning_effort: false,
                    },
                );
            }
            let body_str = serde_json::to_string(&payload).expect("payload serializes");
            http::post_stream(http_client, &url, headers, body_str).await?
        }
        Err(e) => return Err(e),
    };
    let resp = ensure_success(resp).await?;

    let events = Box::pin(sse::sse_events(resp.bytes_stream()));
    let started = Instant::now();
    Ok(Box::pin(futures::stream::unfold(
        (
            events,
            wire_openai::OpenAiStreamAccumulator::new(),
            false,
            started,
        ),
        |(mut events, mut acc, mut done, started)| async move {
            if done {
                return None;
            }
            loop {
                match futures::StreamExt::next(&mut events).await {
                    Some(Ok(SseEvent { data, .. })) => {
                        if data.trim() == "[DONE]" {
                            done = true;
                            break;
                        }
                        let chunk = match sse::parse_json(&data) {
                            Ok(v) => v,
                            Err(e) => {
                                done = true;
                                return Some((Err(e), (events, acc, done, started)));
                            }
                        };
                        if let Some(delta) = acc.feed(&chunk) {
                            return Some((
                                Ok(ChatStreamItem::Delta(delta)),
                                (events, acc, done, started),
                            ));
                        }
                    }
                    Some(Err(e)) => {
                        // Lenient close: content already streamed to the user.
                        done = true;
                        let item = graceful_finish(&mut acc, started, Err(e));
                        return Some((item, (events, acc, done, started)));
                    }
                    None => {
                        done = true;
                        break;
                    }
                }
            }
            let item = graceful_finish(&mut acc, started, Ok(()));
            Some((item, (events, acc, done, started)))
        },
    )))
}

/// Anthropic streaming chat (Messages API SSE) — also used by
/// Vertex-Anthropic via `:rawStreamPredict` (adds `anthropic_version`).
pub(crate) async fn anthropic_sse(
    http_client: &reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    request: &ChatRequest,
) -> Result<ChatStream> {
    let mut payload = wire_anthropic::build_chat_payload(
        request,
        wire_anthropic::AnthropicWireOpts {
            supports_thinking: true,
        },
    );
    if url.contains(":rawStreamPredict") {
        payload["anthropic_version"] = serde_json::json!("vertex-2023-10-16");
    }

    let body_str = serde_json::to_string(&payload).expect("payload serializes");
    let resp = http::post_stream(http_client, &url, headers, body_str).await?;
    let resp = ensure_success(resp).await?;

    let events = Box::pin(sse::sse_events(resp.bytes_stream()));
    let started = Instant::now();
    Ok(Box::pin(futures::stream::unfold(
        (events, AnthropicStreamAccumulator::new(), false, started),
        |(mut events, mut acc, mut done, started)| async move {
            if done {
                return None;
            }
            loop {
                match futures::StreamExt::next(&mut events).await {
                    Some(Ok(SseEvent { event, data })) => {
                        let data_value = match sse::parse_json(&data) {
                            Ok(v) => v,
                            Err(e) => {
                                done = true;
                                return Some((Err(e), (events, acc, done, started)));
                            }
                        };
                        if let Some(delta) = acc.feed(event.as_deref(), &data_value) {
                            return Some((
                                Ok(ChatStreamItem::Delta(delta)),
                                (events, acc, done, started),
                            ));
                        }
                        if event.as_deref() == Some("message_stop") {
                            done = true;
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        done = true;
                        let item = graceful_finish(&mut acc, started, Err(e));
                        return Some((item, (events, acc, done, started)));
                    }
                    None => {
                        done = true;
                        break;
                    }
                }
            }
            let item = graceful_finish(&mut acc, started, Ok(()));
            Some((item, (events, acc, done, started)))
        },
    )))
}

/// Gemini streaming chat (`:streamGenerateContent?alt=sse`).
pub(crate) async fn gemini_sse(
    http_client: &reqwest::Client,
    url: String,
    api_key: &str,
    request: &ChatRequest,
) -> Result<ChatStream> {
    let payload = gemini::build_payload(request);
    let headers = vec![("x-goog-api-key".to_string(), api_key.to_string())];
    let body_str = serde_json::to_string(&payload).expect("payload serializes");
    let resp = http::post_stream(http_client, &url, headers, body_str).await?;
    let resp = ensure_success(resp).await?;

    let events = Box::pin(sse::sse_events(resp.bytes_stream()));
    let started = Instant::now();
    Ok(Box::pin(futures::stream::unfold(
        (
            events,
            gemini::GeminiStreamAccumulator::new(),
            false,
            started,
        ),
        |(mut events, mut acc, mut done, started)| async move {
            if done {
                return None;
            }
            loop {
                match futures::StreamExt::next(&mut events).await {
                    Some(Ok(SseEvent { data, .. })) => {
                        let chunk = match sse::parse_json(&data) {
                            Ok(v) => v,
                            Err(e) => {
                                done = true;
                                return Some((Err(e), (events, acc, done, started)));
                            }
                        };
                        if let Some(delta) = acc.feed(&chunk) {
                            return Some((
                                Ok(ChatStreamItem::Delta(delta)),
                                (events, acc, done, started),
                            ));
                        }
                    }
                    Some(Err(e)) => {
                        done = true;
                        let item = graceful_finish(&mut acc, started, Err(e));
                        return Some((item, (events, acc, done, started)));
                    }
                    None => {
                        done = true;
                        break;
                    }
                }
            }
            let item = graceful_finish(&mut acc, started, Ok(()));
            Some((item, (events, acc, done, started)))
        },
    )))
}

/// On clean EOF/`[DONE]`: always produce the assembled response.
/// On transport error mid-stream: still complete gracefully when the user
/// already received meaningful content; otherwise surface the failure.
fn graceful_finish<A>(acc: &mut A, started: Instant, err: Result<()>) -> Result<ChatStreamItem>
where
    A: GracefulFinish,
{
    let duration_ms = started.elapsed().as_millis() as u64;
    let result = acc.finish_graceful(duration_ms);
    match (result, err) {
        (Ok(resp), _) => Ok(ChatStreamItem::Completed(resp)),
        (Err(parse_err), Err(transport)) => Err(transport_context(transport, parse_err)),
        (Err(parse_err), Ok(())) => Err(parse_err),
    }
}

fn transport_context(transport: CoreError, parse_err: CoreError) -> CoreError {
    CoreError::Parse(format!("{transport}; partial stream: {parse_err}"))
}

trait GracefulFinish {
    fn finish_graceful(&mut self, duration_ms: u64) -> Result<ChatResponse>;
}

impl GracefulFinish for wire_openai::OpenAiStreamAccumulator {
    fn finish_graceful(&mut self, duration_ms: u64) -> Result<ChatResponse> {
        std::mem::take(self).finish(duration_ms)
    }
}

impl GracefulFinish for AnthropicStreamAccumulator {
    fn finish_graceful(&mut self, duration_ms: u64) -> Result<ChatResponse> {
        // finish() consumes self; clone cheap state via Default trick is not
        // possible — so implement take-style via std::mem::take on a
        // dedicated Clone-less type is unavailable. Use a two-phase call:
        std::mem::take(self).finish(duration_ms)
    }
}

impl GracefulFinish for gemini::GeminiStreamAccumulator {
    fn finish_graceful(&mut self, duration_ms: u64) -> Result<ChatResponse> {
        std::mem::take(self).finish(duration_ms)
    }
}
