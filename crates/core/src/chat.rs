use crate::usage::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// Selected reasoning effort for a run. Ordered weakest → strongest.
/// Mirrors the levels models.dev declares in `reasoning_options` (the same
/// source opencode uses): `off/minimal/low/medium/high/xhigh/max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningLevel {
    pub const ALL: [ReasoningLevel; 7] = [
        ReasoningLevel::Off,
        ReasoningLevel::Minimal,
        ReasoningLevel::Low,
        ReasoningLevel::Medium,
        ReasoningLevel::High,
        ReasoningLevel::XHigh,
        ReasoningLevel::Max,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Parse a models.dev `reasoning_options` effort value (or TOML string).
    pub fn parse_effort(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Self::Off,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" | "x-high" => Self::XHigh,
            "max" | "maximum" => Self::Max,
            _ => return None,
        })
    }

    /// Clamp to the closest supported level: the smallest allowed level at
    /// or above `self`, or the largest allowed one when none exists.
    /// `allowed = None` means "no declared limit" → unchanged.
    pub fn clamp_to(self, allowed: Option<&[ReasoningLevel]>) -> ReasoningLevel {
        let Some(allowed) = allowed else {
            return self;
        };
        if allowed.contains(&self) {
            return self;
        }
        if let Some(next) = allowed.iter().copied().find(|l| *l > self) {
            return next;
        }
        allowed.iter().copied().max().unwrap_or(ReasoningLevel::Off)
    }
}

impl fmt::Display for ReasoningLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Neutral tool definition handed to adapters; each provider maps it
/// to its wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing arguments.
    pub parameters: Value,
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    Length,
    Refusal,
    Error,
    Other,
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::ToolUse => "tool_use",
            Self::Length => "length",
            Self::Refusal => "refusal",
            Self::Error => "error",
            Self::Other => "other",
        }
    }
}

/// One message in a provider-neutral conversation.
///
/// `provider_state` carries opaque adapter continuation data (e.g. raw
/// Anthropic content blocks including thinking signatures) so adapters can
/// round-trip their own formats without leaking details into the core.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub tool_calls: Vec<ToolCallRequest>,
    pub tool_call_id: Option<String>,
    /// Name of the invoked function (Gemini's `functionResponse.name`
    /// requires it; OpenAI/Anthropic key off the id alone).
    #[serde(default)]
    pub tool_name: Option<String>,
    pub is_error: bool,
    pub provider_state: Option<Value>,
}

impl Default for ChatMessage {
    fn default() -> Self {
        Self {
            role: Role::User,
            content: String::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            provider_state: None,
        }
    }
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: text.into(),
            ..Default::default()
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: text.into(),
            ..Default::default()
        }
    }

    pub fn assistant_with_tool_calls(
        text: impl Into<String>,
        calls: Vec<ToolCallRequest>,
        provider_state: Option<Value>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: text.into(),
            tool_calls: calls,
            provider_state,
            ..Default::default()
        }
    }

    pub fn tool_result(id: &str, text: impl Into<String>, is_error: bool) -> Self {
        Self {
            role: Role::Tool,
            content: text.into(),
            tool_call_id: Some(id.to_string()),
            tool_name: None,
            is_error,
            ..Default::default()
        }
    }

    /// Tool result carrying the invoked function name (needed by Gemini).
    pub fn named_tool_result(
        id: &str,
        name: &str,
        text: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: text.into(),
            tool_call_id: Some(id.to_string()),
            tool_name: Some(name.to_string()),
            is_error,
            ..Default::default()
        }
    }
}

/// Request for one completion turn.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub reasoning: ReasoningLevel,
    pub max_tokens: u32,
    /// Ask the adapter to use prompt caching when supported.
    /// Adapters must degrade gracefully when it is not.
    pub enable_prompt_cache: bool,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, system: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: system.into(),
            messages: Vec::new(),
            tools: Vec::new(),
            reasoning: ReasoningLevel::Off,
            max_tokens: 16_384,
            enable_prompt_cache: true,
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_messages(mut self, messages: Vec<ChatMessage>) -> Self {
        self.messages = messages;
        self
    }

    pub fn with_reasoning(mut self, level: ReasoningLevel) -> Self {
        self.reasoning = level;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_prompt_cache(mut self, enabled: bool) -> Self {
        self.enable_prompt_cache = enabled;
        self
    }

    /// Convenience for building a single-shot stream from a complete
    /// response (the default `chat_stream` implementation).
    pub fn completed_stream(
        response: ChatResponse,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = crate::Result<ChatStreamItem>> + Send>> {
        Box::pin(futures::stream::once(async move {
            Ok(ChatStreamItem::Completed(response))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_supported_levels() {
        let allowed = [
            ReasoningLevel::Off,
            ReasoningLevel::High,
            ReasoningLevel::Max,
        ];
        assert_eq!(
            ReasoningLevel::Off.clamp_to(Some(&allowed)),
            ReasoningLevel::Off
        );
        assert_eq!(
            ReasoningLevel::High.clamp_to(Some(&allowed)),
            ReasoningLevel::High
        );
        assert_eq!(
            ReasoningLevel::Max.clamp_to(Some(&allowed)),
            ReasoningLevel::Max
        );
    }

    #[test]
    fn clamp_raises_to_next_supported_level() {
        // Model supports only high/max (e.g. deepseek-v4-flash): off/low/medium
        // all clamp up to high.
        let allowed = [ReasoningLevel::High, ReasoningLevel::Max];
        assert_eq!(
            ReasoningLevel::Off.clamp_to(Some(&allowed)),
            ReasoningLevel::High
        );
        assert_eq!(
            ReasoningLevel::Medium.clamp_to(Some(&allowed)),
            ReasoningLevel::High
        );
    }

    #[test]
    fn clamp_lowers_over_requests() {
        // Model caps at high (e.g. mistral-small-4: none/high): max → high.
        let allowed = [ReasoningLevel::Off, ReasoningLevel::High];
        assert_eq!(
            ReasoningLevel::Max.clamp_to(Some(&allowed)),
            ReasoningLevel::High
        );
    }

    #[test]
    fn clamp_ignores_unknown_declarations() {
        assert_eq!(ReasoningLevel::High.clamp_to(None), ReasoningLevel::High);
    }

    #[test]
    fn parses_catalog_effort_names() {
        assert_eq!(
            ReasoningLevel::parse_effort("none"),
            Some(ReasoningLevel::Off)
        );
        assert_eq!(
            ReasoningLevel::parse_effort("xhigh"),
            Some(ReasoningLevel::XHigh)
        );
        assert_eq!(
            ReasoningLevel::parse_effort("max"),
            Some(ReasoningLevel::Max)
        );
        assert_eq!(ReasoningLevel::parse_effort("turbo"), None);
    }
}

/// One incremental piece of a streamed completion (UI-facing only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatDelta {
    Text(String),
    Thinking(String),
}

/// Boxed stream of streaming items (delta…, then exactly one `Completed`).
pub type ChatStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = crate::Result<ChatStreamItem>> + Send>>;

/// One item of a streaming chat exchange. Every stream terminates with
/// exactly one [`ChatStreamItem::Completed`] carrying the fully assembled
/// response — identical to what the non-streaming path returns.
#[derive(Debug, Clone)]
pub enum ChatStreamItem {
    Delta(ChatDelta),
    Completed(ChatResponse),
}

/// Response for one completion turn.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub text: String,
    /// Chain-of-thought / reasoning summary if the provider exposes one.
    pub thinking: Option<String>,
    pub tool_calls: Vec<ToolCallRequest>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    /// Adapter-specific raw assistant payload to be echoed back in history.
    pub raw_assistant_message: Option<Value>,
    /// Non-fatal issues detected by the adapter (retries, fallbacks).
    pub warnings: Vec<String>,
    pub duration_ms: u64,
}
