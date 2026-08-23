//! Shared fixtures for wire-format tests.
#![cfg(test)]

use lmhub_core::{ChatMessage, ChatRequest, ReasoningLevel, Role, ToolCallRequest, ToolSpec};
use serde_json::json;

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
