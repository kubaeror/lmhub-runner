//! Helpers shared by the wire adapters (error classification for the
//! degradation ladders).

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
