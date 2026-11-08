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

use crate::wire_common::truncate_body;

/// Turn a non-success response into the canonical provider error, consuming
/// the body (truncated and scrubbed).
async fn status_error(resp: reqwest::Response) -> CoreError {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    CoreError::Provider(format!(
        "HTTP {}: {}",
        status.as_u16(),
        truncate_body(&text, 600)
    ))
}

/// Copy of the request with reasoning disabled (degradation path when a
/// server rejects `reasoning_effort`).
fn without_reasoning(request: &ChatRequest) -> ChatRequest {
    let mut stripped = request.clone();
    stripped.reasoning = ReasoningLevel::Off;
    stripped
}

/// OpenAI-compatible streaming chat (`chat/completions`, SSE).
/// Sends `stream_options.include_usage`; when the server rejects an optional
/// field with a 4xx that names it, degrades once (strip `stream_options`,
/// drop `reasoning_effort`, or fall back to `max_tokens`) and retries.
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
    let mut degraded = false;
    // Degradations discovered before the stream starts; attached to the final
    // response so the run records them as warnings.
    let mut request_warnings: Vec<String> = Vec::new();

    loop {
        let body_str = serde_json::to_string(&payload).expect("payload serializes");
        let resp = http::post_stream(http_client, &url, headers.clone(), body_str).await?;
        if resp.status().is_success() {
            let events = Box::pin(sse::sse_events(resp.bytes_stream()));
            let started = Instant::now();
            let stream_warnings = request_warnings.clone();
            return Ok(Box::pin(futures::stream::unfold(
                (
                    events,
                    wire_openai::OpenAiStreamAccumulator::new(),
                    false,
                    started,
                ),
                move |(mut events, mut acc, mut done, started)| {
                    let stream_warnings = stream_warnings.clone();
                    async move {
                        if done {
                            return None;
                        }
                        loop {
                            match futures::StreamExt::next(&mut events).await {
                                Some(Ok(SseEvent { data, .. })) => {
                                    if data.trim().is_empty() {
                                        // Comment/keep-alive-only block — not an
                                        // event (would otherwise fail JSON parsing
                                        // and abort a healthy stream).
                                        continue;
                                    }
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
                                    if let Some(msg) = sse::framed_error(&chunk) {
                                        done = true;
                                        return Some((
                                            Err(CoreError::Provider(msg)),
                                            (events, acc, done, started),
                                        ));
                                    }
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
                                    let item = graceful_finish_with_warnings(
                                        &mut acc,
                                        started,
                                        Err(e),
                                        stream_warnings,
                                    );
                                    return Some((item, (events, acc, done, started)));
                                }
                                None => {
                                    done = true;
                                    break;
                                }
                            }
                        }
                        let item = graceful_finish_with_warnings(
                            &mut acc,
                            started,
                            Ok(()),
                            stream_warnings,
                        );
                        Some((item, (events, acc, done, started)))
                    }
                },
            )));
        }
        if degraded {
            return Err(status_error(resp).await);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let lower = text.to_ascii_lowercase();
        let strip_usage = lower.contains("stream_options");
        let reasoning_rejected =
            wire_openai::is_reasoning_rejection(&text) && request.reasoning != ReasoningLevel::Off;
        // A rejection naming a concrete level ("please use low, high, or
        // max") means the model cannot disable thinking: retry with the
        // suggested level instead of stripping reasoning entirely.
        let suggested = wire_openai::suggested_reasoning_level(&text);
        let retry_suggested =
            reasoning_rejected && suggested.is_some() && suggested != Some(request.reasoning);
        let use_max_tokens = lower.contains("max_completion_tokens");
        if retry_suggested {
            let mut retried = request.clone();
            retried.reasoning = suggested.unwrap();
            payload = wire_openai::build_stream_payload(
                &retried,
                OpenAiWireOpts {
                    include_reasoning_effort: true,
                },
            );
        } else if strip_usage {
            wire_openai::strip_stream_options(&mut payload);
        } else if reasoning_rejected {
            payload = wire_openai::build_stream_payload(
                &without_reasoning(request),
                OpenAiWireOpts {
                    include_reasoning_effort: false,
                },
            );
        } else if use_max_tokens {
            wire_openai::use_max_tokens_field(&mut payload);
        } else if crate::wire_common::is_tools_rejection(&text) && !request.tools.is_empty() {
            // Model/route has no tool support: one plain-text turn instead of
            // failing the run. The agent loop ends after it (no tool calls).
            let mut stripped = request.clone();
            stripped.tools.clear();
            payload = wire_openai::build_stream_payload(
                &stripped,
                OpenAiWireOpts {
                    include_reasoning_effort: request.reasoning != ReasoningLevel::Off,
                },
            );
            request_warnings.push(
                "provider rejected `tools`; retried without tools (agent loop disabled)".into(),
            );
        } else {
            return Err(CoreError::Provider(format!(
                "HTTP {}: {}",
                status.as_u16(),
                truncate_body(&text, 600)
            )));
        }
        degraded = true;
    }
}

/// Anthropic streaming chat (Messages API SSE) — also used by
/// Vertex-Anthropic via `:rawStreamPredict` (adds `anthropic_version`).
/// Degrades once without the `thinking` config when a server rejects it.
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
    let mut degraded = false;
    let mut request_warnings: Vec<String> = Vec::new();

    loop {
        let body_str = serde_json::to_string(&payload).expect("payload serializes");
        let resp = http::post_stream(http_client, &url, headers.clone(), body_str).await?;
        if resp.status().is_success() {
            let events = Box::pin(sse::sse_events(resp.bytes_stream()));
            let started = Instant::now();
            let stream_warnings = request_warnings.clone();
            return Ok(Box::pin(futures::stream::unfold(
                (events, AnthropicStreamAccumulator::new(), false, started),
                move |(mut events, mut acc, mut done, started)| {
                    let stream_warnings = stream_warnings.clone();
                    async move {
                        if done {
                            return None;
                        }
                        loop {
                            match futures::StreamExt::next(&mut events).await {
                                Some(Ok(SseEvent { event, data })) => {
                                    if event.as_deref() == Some("error") {
                                        done = true;
                                        return Some((
                                            Err(CoreError::Provider(sse::error_from_text(&data))),
                                            (events, acc, done, started),
                                        ));
                                    }
                                    let data_value = match sse::parse_json(&data) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            done = true;
                                            return Some((Err(e), (events, acc, done, started)));
                                        }
                                    };
                                    if let Some(msg) = sse::framed_error(&data_value) {
                                        done = true;
                                        return Some((
                                            Err(CoreError::Provider(msg)),
                                            (events, acc, done, started),
                                        ));
                                    }
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
                                    let item = graceful_finish_with_warnings(
                                        &mut acc,
                                        started,
                                        Err(e),
                                        stream_warnings,
                                    );
                                    return Some((item, (events, acc, done, started)));
                                }
                                None => {
                                    done = true;
                                    break;
                                }
                            }
                        }
                        let item = graceful_finish_with_warnings(
                            &mut acc,
                            started,
                            Ok(()),
                            stream_warnings,
                        );
                        Some((item, (events, acc, done, started)))
                    }
                },
            )));
        }
        if degraded {
            return Err(status_error(resp).await);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let thinking_rejected =
            wire_openai::is_reasoning_rejection(&text) && request.reasoning != ReasoningLevel::Off;
        if crate::wire_common::is_tools_rejection(&text) && !request.tools.is_empty() {
            // No tool support on this route: try one plain-text turn.
            let mut stripped = request.clone();
            stripped.tools.clear();
            payload = wire_anthropic::build_chat_payload(
                &stripped,
                wire_anthropic::AnthropicWireOpts {
                    supports_thinking: request.reasoning != ReasoningLevel::Off,
                },
            );
            if url.contains(":rawStreamPredict") {
                payload["anthropic_version"] = serde_json::json!("vertex-2023-10-16");
            }
            request_warnings.push(
                "provider rejected `tools`; retried without tools (agent loop disabled)".into(),
            );
            degraded = true;
            continue;
        }
        if !thinking_rejected {
            return Err(CoreError::Provider(format!(
                "HTTP {}: {}",
                status.as_u16(),
                truncate_body(&text, 600)
            )));
        }
        // Retry with the level the provider suggests when it names one
        // ("cannot be disabled; please use low, high, or max"); otherwise
        // strip the thinking config entirely.
        let mut retried = request.clone();
        retried.reasoning =
            wire_openai::suggested_reasoning_level(&text).unwrap_or(ReasoningLevel::Off);
        payload = wire_anthropic::build_chat_payload(
            &retried,
            wire_anthropic::AnthropicWireOpts {
                supports_thinking: retried.reasoning != ReasoningLevel::Off,
            },
        );
        if url.contains(":rawStreamPredict") {
            payload["anthropic_version"] = serde_json::json!("vertex-2023-10-16");
        }
        degraded = true;
    }
}

/// Gemini streaming chat (`:streamGenerateContent?alt=sse`).
/// Auth headers are supplied by the caller (`x-goog-api-key` for the native
/// API, `Authorization: Bearer` on Vertex AI).
pub(crate) async fn gemini_sse(
    http_client: &reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    request: &ChatRequest,
) -> Result<ChatStream> {
    let mut payload = gemini::build_payload(request);
    let mut degraded = false;
    let mut request_warnings: Vec<String> = Vec::new();
    let resp = loop {
        let body_str = serde_json::to_string(&payload).expect("payload serializes");
        let resp = http::post_stream(http_client, &url, headers.clone(), body_str).await?;
        if resp.status().is_success() {
            break resp;
        }
        if degraded {
            return Err(status_error(resp).await);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !gemini::is_thinking_rejection(&text) {
            return Err(CoreError::Provider(format!(
                "HTTP {}: {}",
                status.as_u16(),
                truncate_body(&text, 600)
            )));
        }
        // Degrade once: retry without thinkingConfig.
        payload = gemini::build_payload_without_thinking(request);
        request_warnings.push("provider rejected `thinkingConfig`; retried without it".into());
        degraded = true;
    };

    let events = Box::pin(sse::sse_events(resp.bytes_stream()));
    let started = Instant::now();
    let stream_warnings = request_warnings.clone();
    Ok(Box::pin(futures::stream::unfold(
        (
            events,
            gemini::GeminiStreamAccumulator::new(),
            false,
            started,
        ),
        move |(mut events, mut acc, mut done, started)| {
            let stream_warnings = stream_warnings.clone();
            async move {
                if done {
                    return None;
                }
                loop {
                    match futures::StreamExt::next(&mut events).await {
                        Some(Ok(SseEvent { data, .. })) => {
                            if data.trim().is_empty() {
                                // Comment/keep-alive-only block — not an event.
                                continue;
                            }
                            let chunk = match sse::parse_json(&data) {
                                Ok(v) => v,
                                Err(e) => {
                                    done = true;
                                    return Some((Err(e), (events, acc, done, started)));
                                }
                            };
                            if let Some(msg) = sse::framed_error(&chunk) {
                                done = true;
                                return Some((
                                    Err(CoreError::Provider(msg)),
                                    (events, acc, done, started),
                                ));
                            }
                            if let Some(delta) = acc.feed(&chunk) {
                                return Some((
                                    Ok(ChatStreamItem::Delta(delta)),
                                    (events, acc, done, started),
                                ));
                            }
                        }
                        Some(Err(e)) => {
                            done = true;
                            let item = graceful_finish_with_warnings(
                                &mut acc,
                                started,
                                Err(e),
                                stream_warnings,
                            );
                            return Some((item, (events, acc, done, started)));
                        }
                        None => {
                            done = true;
                            break;
                        }
                    }
                }
                let item =
                    graceful_finish_with_warnings(&mut acc, started, Ok(()), stream_warnings);
                Some((item, (events, acc, done, started)))
            }
        },
    )))
}

/// On clean EOF/`[DONE]`: always produce the assembled response.
/// On transport error mid-stream: still complete gracefully when the user
/// already received meaningful content; otherwise surface the failure.
///
/// Idle read timeouts are the exception: a stalled stream means the answer is
/// incomplete, so the run must see [`CoreError::Timeout`] instead of a
/// fabricated `Completed`.
fn graceful_finish<A>(acc: &mut A, started: Instant, err: Result<()>) -> Result<ChatStreamItem>
where
    A: GracefulFinish,
{
    if matches!(err, Err(CoreError::Timeout)) {
        return Err(CoreError::Timeout);
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    let result = acc.finish_graceful(duration_ms);
    match (result, err) {
        (Ok(resp), _) => Ok(ChatStreamItem::Completed(resp)),
        (Err(parse_err), Err(transport)) => Err(transport_context(transport, parse_err)),
        (Err(parse_err), Ok(())) => Err(parse_err),
    }
}

/// [`graceful_finish`] plus request-level degradation warnings (e.g. "retried
/// without tools") attached to the assembled response.
fn graceful_finish_with_warnings<A>(
    acc: &mut A,
    started: Instant,
    err: Result<()>,
    warnings: Vec<String>,
) -> Result<ChatStreamItem>
where
    A: GracefulFinish,
{
    match graceful_finish(acc, started, err)? {
        ChatStreamItem::Completed(mut resp) => {
            resp.warnings.extend(warnings);
            Ok(ChatStreamItem::Completed(resp))
        }
        other => Ok(other),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire_test_util::sample_request;
    use lmhub_core::{ChatDelta, ChatStreamItem};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Read one HTTP request (headers + Content-Length body) from a socket.
    async fn read_http_request(sock: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            sock.read_exact(&mut byte).await.unwrap();
            buf.push(byte[0]);
            if buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let len = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        sock.read_exact(&mut body).await.unwrap();
        format!("{head}{}", String::from_utf8_lossy(&body))
    }

    const REJECT_BODY: &str = r#"{"error":{"type":"server_error","message":"Error from provider (Console): Upstream request failed: [1210] This model always engages in thinking and cannot be disabled; please use low, high, or max"}}"#;

    /// First request → 400 with the reasoning-rejection body; second request
    /// → 200 SSE stream. Both request bodies are sent back via the channel.
    async fn mock_reject_then_stream() -> (String, tokio::sync::oneshot::Receiver<(String, String)>)
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut s1, _) = listener.accept().await.unwrap();
            let req1 = read_http_request(&mut s1).await;
            let resp1 = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                REJECT_BODY.len(),
                REJECT_BODY
            );
            s1.write_all(resp1.as_bytes()).await.unwrap();

            let (mut s2, _) = listener.accept().await.unwrap();
            let req2 = read_http_request(&mut s2).await;
            let sse = concat!(
                "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            let resp2 = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            s2.write_all(resp2.as_bytes()).await.unwrap();
            let _ = tx.send((req1, req2));
        });
        (format!("http://{addr}/v1/chat/completions"), rx)
    }

    #[tokio::test]
    async fn reasoning_rejection_retries_with_suggested_level() {
        let (url, rx) = mock_reject_then_stream().await;
        let client = reqwest::Client::new();
        let req = sample_request(ReasoningLevel::Medium);
        let stream = openai_sse(&client, url, Vec::new(), &req)
            .await
            .expect("ladder retry succeeds");
        let deltas: Vec<Result<ChatStreamItem>> = futures::StreamExt::collect(stream).await;
        let text: String = deltas
            .iter()
            .filter_map(|d| match d {
                Ok(ChatStreamItem::Delta(ChatDelta::Text(t))) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");

        let (req1, req2) = rx.await.expect("mock served both requests");
        assert!(
            req1.contains("\"reasoning_effort\":\"medium\""),
            "first attempt keeps the requested level: {req1}"
        );
        assert!(
            req2.contains("\"reasoning_effort\":\"low\""),
            "retry uses the suggested level: {req2}"
        );
    }

    /// A stream that stalls longer than the idle window must surface
    /// `CoreError::Timeout`, never a fabricated `Completed` response.
    #[tokio::test]
    async fn idle_timeout_stall_is_timeout_not_completed() {
        crate::http::init_stream_idle_timeout(std::time::Duration::from_secs(1));

        let part1 =
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n";
        let part2 = "data: [DONE]\n\n";
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut sock).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                part1.len() + part2.len()
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(part1.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let _ = sock.write_all(part2.as_bytes()).await; // client may be gone
        });

        let client = reqwest::Client::new();
        let req = sample_request(ReasoningLevel::Off);
        let stream = openai_sse(
            &client,
            format!("http://{addr}/v1/chat/completions"),
            Vec::new(),
            &req,
        )
        .await
        .unwrap();
        let items: Vec<Result<ChatStreamItem>> = futures::StreamExt::collect(stream).await;

        assert!(
            items.iter().any(|i| matches!(i, Err(CoreError::Timeout))),
            "stalled stream must end in a timeout: {items:?}"
        );
        assert!(
            !items
                .iter()
                .any(|i| matches!(i, Ok(ChatStreamItem::Completed(_)))),
            "a stalled stream must not report Completed: {items:?}"
        );
        // Content that actually arrived is still delivered to the UI.
        assert!(items.iter().any(|i| matches!(
            i,
            Ok(ChatStreamItem::Delta(ChatDelta::Text(t))) if t == "Hel"
        )));
    }

    /// Model without tool support: the runner retries the stream once
    /// without `tools` and records a warning on the assembled response.
    #[tokio::test]
    async fn openai_tools_rejection_retries_without_tools() {
        let reject = serde_json::json!({
            "error": {"type": "invalid_request_error",
                      "message": "Invalid parameter: 'tools' is not supported by this model"}
        })
        .to_string();
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"plain answer\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string();
        let (base, rx) = crate::wire_test_util::two_shot_server(400, reject, sse).await;

        let client = reqwest::Client::new();
        let req = sample_request(ReasoningLevel::Off);
        let stream = openai_sse(
            &client,
            format!("{base}/chat/completions"),
            Vec::new(),
            &req,
        )
        .await
        .expect("tool rejection must degrade, not fail");
        let items: Vec<Result<ChatStreamItem>> = futures::StreamExt::collect(stream).await;
        let completed = items
            .iter()
            .find_map(|i| match i {
                Ok(ChatStreamItem::Completed(resp)) => Some(resp),
                _ => None,
            })
            .expect("completed response");
        assert_eq!(completed.text, "plain answer");
        assert!(
            completed.warnings.iter().any(|w| w.contains("tools")),
            "warning must be recorded: {:?}",
            completed.warnings
        );

        let (first, second) = rx.await.unwrap();
        assert!(first.contains("\"tools\""), "first attempt keeps tools");
        assert!(
            !second.contains("\"tools\""),
            "retry must strip tools: {second}"
        );
    }

    /// Anthropic `event: error` frames abort the stream with the provider's
    /// message instead of being silently ignored.
    #[tokio::test]
    async fn anthropic_error_event_surfaces_provider_error() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        )
        .to_string();
        let (base, _rx) =
            crate::wire_test_util::one_shot_server(200, "text/event-stream", sse).await;
        let stream = anthropic_sse(
            &reqwest::Client::new(),
            format!("{base}/messages"),
            Vec::new(),
            &sample_request(ReasoningLevel::Off),
        )
        .await
        .unwrap();
        let items: Vec<Result<ChatStreamItem>> = futures::StreamExt::collect(stream).await;
        assert!(
            items
                .iter()
                .any(|i| matches!(i, Err(CoreError::Provider(m)) if m.contains("Overloaded"))),
            "{items:?}"
        );
    }

    /// Framed `data: {"error": …}` payloads are errors in all three wires.
    #[tokio::test]
    async fn gemini_framed_error_surfaces_provider_error() {
        let sse = "data: {\"error\":{\"code\":429,\"message\":\"Quota exceeded\"}}\n\n".to_string();
        let (base, _rx) =
            crate::wire_test_util::one_shot_server(200, "text/event-stream", sse).await;
        let client = reqwest::Client::new();
        let req = sample_request(ReasoningLevel::Off);
        let stream = gemini_sse(
            &client,
            format!("{base}/models/test:streamGenerateContent?alt=sse"),
            Vec::new(),
            &req,
        )
        .await
        .unwrap();
        let items: Vec<Result<ChatStreamItem>> = futures::StreamExt::collect(stream).await;
        assert!(
            items
                .iter()
                .any(|i| matches!(i, Err(CoreError::Provider(m)) if m.contains("Quota exceeded"))),
            "{items:?}"
        );

        // Same for the OpenAI wire.
        let sse = "data: {\"error\":{\"message\":\"rate limited\"}}\n\n".to_string();
        let (base, _rx) =
            crate::wire_test_util::one_shot_server(200, "text/event-stream", sse).await;
        let stream = openai_sse(
            &client,
            format!("{base}/chat/completions"),
            Vec::new(),
            &req,
        )
        .await
        .unwrap();
        let items: Vec<Result<ChatStreamItem>> = futures::StreamExt::collect(stream).await;
        assert!(
            items
                .iter()
                .any(|i| matches!(i, Err(CoreError::Provider(m)) if m.contains("rate limited"))),
            "{items:?}"
        );
    }
}
