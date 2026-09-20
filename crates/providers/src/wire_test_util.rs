//! Shared fixtures for wire-format tests.
#![cfg(test)]

use lmhub_core::{ChatMessage, ChatRequest, ReasoningLevel, Role, ToolCallRequest, ToolSpec};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub(crate) fn sample_request(reasoning: ReasoningLevel) -> ChatRequest {
    let mut req = ChatRequest::new("test-model", "You are a coding agent.");
    req.max_tokens = 1024;
    req.reasoning = reasoning;
    req.messages = vec![
        ChatMessage::user("build the app"),
        ChatMessage {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCallRequest {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "a.txt"}),
            }],
            ..Default::default()
        },
    ];
    req.tools = vec![ToolSpec {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
    }];
    req
}

/// One-shot HTTP/1.1 server for adapter tests.
///
/// Serves a single request with `status`/`content_type`/`body` and returns
/// the raw request (headers + body) through the receiver, so tests can assert
/// exactly which credential header was sent. The runner is not exercised here:
/// tests that need multiple requests spawn their own listener.
pub(crate) async fn one_shot_server(
    status: u16,
    content_type: &str,
    body: String,
) -> (String, tokio::sync::oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let content_type = content_type.to_string();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let raw = read_http_request(&mut sock).await;
        let reason = if status == 200 { "OK" } else { "Error" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(response.as_bytes()).await.unwrap();
        let _ = tx.send(raw);
    });
    (format!("http://{addr}/v1"), rx)
}

async fn read_http_request(sock: &mut tokio::net::TcpStream) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if sock.read_exact(&mut byte).await.is_err() {
            break;
        }
        buf.push(byte[0]);
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
    if sock.read_exact(&mut body).await.is_ok() {
        buf.extend_from_slice(&body);
    }
    String::from_utf8_lossy(&buf).to_string()
}
