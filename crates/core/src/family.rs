/// Derive the output directory family for a model.
///
/// Priority: explicit config value > Models.dev metadata > heuristic
/// derived from the model id prefix. Never invents exotic names — falls
/// back to the capitalized first id segment.
pub fn infer_family(model_id: &str, explicit: Option<&String>) -> String {
    if let Some(f) = explicit {
        let f = f.trim();
        if !f.is_empty() {
            return sanitize(f);
        }
    }
    from_id(model_id)
}

pub fn from_id(model_id: &str) -> String {
    let id = model_id.to_ascii_lowercase();
    const TABLE: &[(&str, &str)] = &[
        ("claude", "Claude"),
        ("gpt", "GPT"),
        ("chatgpt", "GPT"),
        ("glm", "GLM"),
        ("gemini", "Gemini"),
        ("gemma", "Gemini"),
        ("qwen", "Qwen"),
        ("deepseek", "DeepSeek"),
        ("minimax", "MiniMax"),
        ("abab", "MiniMax"),
        ("kimi", "Kimi"),
        ("moonshot", "Kimi"),
        ("llama", "Llama"),
        ("mistral", "Mistral"),
        ("mixtral", "Mistral"),
        ("grok", "Grok"),
        ("command", "Cohere"),
        ("phi", "Phi"),
        ("nova", "Nova"),
        ("o1", "GPT"),
        ("o3", "GPT"),
        ("o4", "GPT"),
    ];

    // Split into tokens so "org/gpt-4o" hits "gpt" while "mygpt-model"
    // does not. A token also matches when the remainder after the prefix
    // is purely numeric ("qwen3" -> Qwen).
    for token in id.split(['-', '_', '/', '.', '+', ':']) {
        for (prefix, family) in TABLE {
            if token == *prefix {
                return (*family).to_string();
            }
            match token.strip_prefix(prefix) {
                Some(rest)
                    if !rest.is_empty()
                        && rest.chars().all(|c| c.is_ascii_digit())
                        && prefix.len() >= 3 =>
                {
                    return (*family).to_string();
                }
                _ => {}
            }
        }
    }
    let segment = model_id
        .split(['-', '_', '/', '.'])
        .find(|s| !s.is_empty())
        .unwrap_or("unknown");
    sanitize(segment)
}

/// Sanitize a single path component (model/reasoning directory names).
/// Preserves the original casing (`claude-sonnet-4`, `high`) — unlike
/// family names, these mirror the model id verbatim.
pub fn sanitize_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').trim().to_string();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed
    }
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == ' ' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').trim().to_string();
    match trimmed.chars().next() {
        None => "Unknown".to_string(),
        Some(first) => {
            let mut out: String = first.to_ascii_uppercase().to_string();
            out.push_str(&trimmed[first.len_utf8()..]);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_prefixes() {
        assert_eq!(from_id("claude-sonnet-4"), "Claude");
        assert_eq!(from_id("gpt-5.2-turbo"), "GPT");
        assert_eq!(from_id("glm-4.7-air"), "GLM");
        assert_eq!(from_id("deepseek-chat-v4"), "DeepSeek");
        assert_eq!(from_id("qwen3-coder-plus"), "Qwen");
        assert_eq!(from_id("MiniMax-M2"), "MiniMax");
        assert_eq!(from_id("gemini-3-pro"), "Gemini");
    }

    #[test]
    fn boundary_matching() {
        assert_ne!(from_id("mygpt-model"), "GPT");
        assert_eq!(from_id("org/gpt-4o"), "GPT");
    }

    #[test]
    fn fallback_capitalizes_first_segment() {
        assert_eq!(from_id("foo-bar-1"), "Foo");
        assert_eq!(infer_family("x/yz", None), "X");
    }

    #[test]
    fn explicit_wins() {
        assert_eq!(
            infer_family("anything", Some(&"My Family".to_string())),
            "My Family"
        );
    }
}
