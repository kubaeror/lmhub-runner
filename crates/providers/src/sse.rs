//! Minimal SSE (Server-Sent Events) line parser over a byte stream.
//!
//! Handles the subset every supported provider uses: `event:`/`data:` fields,
//! multi-line `data` joined with `\n`, comment lines starting with `:`, and
//! the OpenAI-style `data: [DONE]` sentinel (passed through verbatim —
//! protocol layers decide what it means).

use futures::StreamExt;
use lmhub_core::{CoreError, Result};
use serde_json::Value;

#[derive(Debug, Clone)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Transform an HTTP body byte stream into parsed SSE events.
pub(crate) fn sse_events(
    body: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
) -> impl futures::Stream<Item = Result<SseEvent>> + Send {
    // bytes is a transitive dependency of reqwest; pin the version we use.
    futures::stream::unfold(
        State {
            body: Box::pin(body),
            buf: Vec::new(),
            eof: false,
        },
        |mut state| async move {
            loop {
                if let Some(event) = take_event(&mut state.buf) {
                    return Some((Ok(event), state));
                }
                if state.eof {
                    // Flush a trailing unterminated block (lenient servers).
                    if let Some(event) = take_final(&mut state.buf) {
                        return Some((Ok(event), state));
                    }
                    return None;
                }
                match state.body.as_mut().next().await {
                    Some(Ok(chunk)) => state.buf.extend_from_slice(&chunk),
                    Some(Err(e)) => {
                        state.eof = true;
                        let err = CoreError::Http(redact_safe(&e.to_string()));
                        return Some((Err(err), state));
                    }
                    None => state.eof = true,
                }
            }
        },
    )
}

struct State {
    body: std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: Vec<u8>,
    eof: bool,
}

fn redact_safe(s: &str) -> String {
    lmhub_core::redact::scrub(s)
}

/// Extract one complete event block (terminated by a blank line) if present.
fn take_event(buf: &mut Vec<u8>) -> Option<SseEvent> {
    for sep in [&b"\r\n\r\n"[..], &b"\n\n"[..]] {
        if let Some(pos) = find(buf, sep) {
            let block: Vec<u8> = buf.drain(..pos + sep.len()).collect();
            let text = String::from_utf8_lossy(&block[..block.len() - sep.len()]).to_string();
            return Some(block_to_event(&text));
        }
    }
    None
}

fn take_final(buf: &mut Vec<u8>) -> Option<SseEvent> {
    if buf.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(buf).to_string();
    buf.clear();
    Some(block_to_event(&text))
}

fn block_to_event(block: &str) -> SseEvent {
    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix(':') {
            let _ = rest; // comment
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim_start_matches(' ').to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    SseEvent {
        event,
        data: data_lines.join("\n"),
    }
}

/// Parse one SSE `data` payload as JSON (convenience for protocol layers).
pub(crate) fn parse_json(data: &str) -> Result<Value> {
    serde_json::from_str(data)
        .map_err(|e| CoreError::Parse(format!("invalid SSE JSON ({e}): {}", truncate(data))))
}

fn truncate(s: &str) -> String {
    s.chars().take(200).collect()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn stream_of(
        chunks: Vec<String>,
    ) -> impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> {
        futures::stream::iter(chunks.into_iter().map(|c| Ok(bytes::Bytes::from(c))))
    }

    #[tokio::test]
    async fn parses_basic_sequence() {
        let body = stream_of(vec![
            "event: foo\ndata: {\"a\":1}\n\ndata: {\"b\":2}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ]);
        let events: Vec<SseEvent> = sse_events(body).map(|e| e.unwrap()).collect().await;
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event.as_deref(), Some("foo"));
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].event, None);
        assert_eq!(events[2].data, "[DONE]");
    }

    #[tokio::test]
    async fn joins_multi_line_data_and_splits_chunks() {
        let body = stream_of(vec![
            "data: line1\nda".to_string(),
            "ta: line2\n\ndata: x\n\n".to_string(),
        ]);
        let events: Vec<SseEvent> = sse_events(body).map(|e| e.unwrap()).collect().await;
        assert_eq!(events[0].data, "line1\nline2");
        assert_eq!(events[1].data, "x");
    }

    #[tokio::test]
    async fn ignores_comments_and_flushes_trailing_block() {
        let body = stream_of(vec![
            ": keep-alive\ndata: y\n\n".to_string(),
            "data: tail".to_string(), // no final blank line
        ]);
        let events: Vec<SseEvent> = sse_events(body).map(|e| e.unwrap()).collect().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "y");
        assert_eq!(events[1].data, "tail");
    }

    #[test]
    fn json_parse_error_is_parse_kind() {
        let err = parse_json("nope").unwrap_err();
        assert!(matches!(err, CoreError::Parse(_)));
    }
}
