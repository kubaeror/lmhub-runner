//! Helpers shared by the wire adapters (error classification for the
//! degradation ladders, warning merging, percent-encoding, body excerpts).

use lmhub_core::{ChatResponse, ReasoningLevel};

/// Prepend adapter-level degradation warnings to a response's own warnings.
pub(crate) fn merge_warnings(mut resp: ChatResponse, warnings: Vec<String>) -> ChatResponse {
    let mut all = warnings;
    all.append(&mut resp.warnings);
    resp.warnings = all;
    resp
}

/// RFC 3986 unreserved-set percent-encoding (path/query/deployment ids).
pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Scrubbed, char-safe body excerpt for error messages.
pub(crate) fn truncate_body(body: &str, max_chars: usize) -> String {
    let scrubbed = lmhub_core::redact::scrub(body);
    let mut s: String = scrubbed.chars().take(max_chars).collect();
    if scrubbed.chars().count() > max_chars {
        s.push('…');
    }
    s
}

/// Broad reasoning-rejection detection: a 4xx explaining that the request's
/// reasoning effort / thinking mode is not acceptable for this model
/// (e.g. "reasoning_effort" rejected, or "always engages in thinking and
/// cannot be disabled; please use low, high, or max"). These must trigger a
/// retry, not a hard failure.
pub(crate) fn is_reasoning_rejection(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    if m.contains("reasoning_effort") {
        return true;
    }
    let reasoning_clause = m.contains("reasoning")
        && (m.contains("unsupported")
            || m.contains("not supported")
            || m.contains("invalid")
            || m.contains("disabled")
            || m.contains("available")
            || m.contains("cannot"));
    let thinking_clause = m.contains("thinking")
        && (m.contains("unsupported")
            || m.contains("not supported")
            || m.contains("disabled")
            || m.contains("cannot")
            || m.contains("always"));
    let effort_clause = m.contains("effort")
        && (m.contains("unsupported") || m.contains("invalid") || m.contains("please use"));
    reasoning_clause || thinking_clause || effort_clause
}

/// First concrete reasoning level a rejection message suggests, e.g.
/// "please use low, high, or max" → Low. `None` when the message names no
/// level.
///
/// Matching is on whole tokens (split on non-alphanumerics): substring
/// matches inside `allowed`, `offered`, `maximum`, `below` or `slow` must
/// not be read as levels. The earliest named token wins.
pub(crate) fn suggested_reasoning_level(msg: &str) -> Option<ReasoningLevel> {
    for token in msg.split(|c: char| !c.is_alphanumeric()) {
        let level = match token.to_ascii_lowercase().as_str() {
            "xhigh" => ReasoningLevel::XHigh,
            "minimal" => ReasoningLevel::Minimal,
            "medium" => ReasoningLevel::Medium,
            "high" => ReasoningLevel::High,
            // "maximum" is its own token and intentionally not a level name:
            // rejecting it avoids the old substring false positive.
            "max" => ReasoningLevel::Max,
            "low" => ReasoningLevel::Low,
            "none" | "off" => ReasoningLevel::Off,
            _ => continue,
        };
        return Some(level);
    }
    None
}

/// True when a provider error says tool/function calling is unsupported for
/// this route, so the request can be retried once without `tools`.
///
/// Kept token-tolerant: providers word it as `'tools'`, `"tools"`,
/// `tool_calls`, `tool use is not supported`, `function calling is not
/// supported`, etc.
pub(crate) fn is_tools_rejection(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    if m.contains("'tools'")
        || m.contains("\"tools\"")
        || m.contains("tool_calls")
        || m.contains("tool use is not supported")
    {
        return true;
    }
    let names_tools = m.contains("tools")
        || m.contains("tool calling")
        || m.contains("function calling")
        || m.contains("tool use");
    let unsupported = m.contains("unsupported")
        || m.contains("not supported")
        || m.contains("not available")
        || m.contains("does not support")
        || m.contains("doesn't support");
    names_tools && unsupported
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_common_wording() {
        for msg in [
            "Invalid parameter: 'tools' is not supported by this model",
            "tools are not supported for this route",
            "This model does not support function calling",
            "tool_calls is an unrecognized field",
            "The requested tool use is unsupported",
        ] {
            assert!(is_tools_rejection(msg), "{msg}");
        }
        for msg in [
            "invalid api key",
            "thinking is not supported",
            "tool result too large",
            "max_completion_tokens is not supported, use max_tokens",
        ] {
            assert!(!is_tools_rejection(msg), "{msg}");
        }
    }
}
