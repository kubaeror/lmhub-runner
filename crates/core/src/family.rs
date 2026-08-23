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
        ("codex", "GPT"),
        ("o1", "GPT"),
        ("o3", "GPT"),
        ("o4", "GPT"),
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
        ("ministral", "Mistral"),
        ("devstral", "Mistral"),
        ("codestral", "Mistral"),
        ("magistral", "Mistral"),
        ("pixtral", "Mistral"),
        ("voxtral", "Mistral"),
        ("grok", "Grok"),
        ("command", "Cohere"),
        ("north", "Cohere"),
        ("phi", "Phi"),
        ("nova", "Nova"),
        ("nemotron", "Nemotron"),
        ("seed", "Seed"),
        ("inkling", "Inkling"),
        ("muse", "Muse"),
        ("sonar", "Sonar"),
        ("ernie", "Ernie"),
        ("hunyuan", "Hunyuan"),
        ("hy3", "Hunyuan"),
        ("hy", "Hunyuan"),
        ("step", "Step"),
        ("granite", "Granite"),
        ("trinity", "Trinity"),
        ("jamba", "Jamba"),
        ("palmyra", "Palmyra"),
        ("laguna", "Laguna"),
        ("mimo", "Mimo"),
        ("hermes", "Hermes"),
        ("mercury", "Mercury"),
        ("sarvam", "Sarvam"),
        ("liquid", "Liquid"),
        ("ring", "Ring"),
        ("ling", "Ling"),
        ("kat", "Kat"),
        ("baichuan", "Baichuan"),
        ("reka", "Reka"),
        ("solar", "Solar"),
        ("olmo", "Olmo"),
        ("alpha", "Alpha"),
        ("agi", "Agi"),
        ("yi", "Yi"),
        ("longcat", "Longcat"),
        ("ornith", "Ornith"),
        ("lucidquery", "Lucid"),
        ("qvq", "Qvq"),
        ("venice", "Venice"),
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

    /// Representative model ids straight from the models.dev catalog (the
    /// fallback matters when catalog metadata is unavailable) — every chat
    /// family must land on a canonical name, never a vendor prefix.
    #[test]
    fn catalog_families_have_canonical_names() {
        let cases: &[(&str, &str)] = &[
            ("anthropic/claude-opus-4.7", "Claude"),
            ("@cf/meta/llama-3.1-8b-instruct-fp8", "Llama"),
            ("google/gemini-3.1-pro-preview", "Gemini"),
            ("@cf/nvidia/nemotron-3-120b-a12b", "Nemotron"),
            ("bytedance/seed-oss-36b-instruct", "Seed"),
            ("thinkingmachines/inkling", "Inkling"),
            ("meta/muse-glimmer-30b", "Muse"),
            ("mistralai/Ministral-3-14B-Instruct-2512", "Mistral"),
            ("tencent/hy3", "Hunyuan"),
            ("devstral-2512", "Mistral"),
            ("mistralai/codestral-2508", "Mistral"),
            ("mistral/magistral-medium-latest", "Mistral"),
            ("pixtral-12b-2409", "Mistral"),
            ("sonar-deep-research", "Sonar"),
            ("ernie-5.0-thinking-preview", "Ernie"),
            ("upstage/solar-pro-3", "Solar"),
            ("@cf/ibm-granite/granite-4.0-h-micro", "Granite"),
            ("arcee-ai/trinity-large-thinking", "Trinity"),
            ("nex-agi/nex-n2-mini", "Agi"),
            ("jamba-large-1.6", "Jamba"),
            ("writer/palmyra-x5", "Palmyra"),
            ("poolside/laguna-xs-2.1", "Laguna"),
            ("xiaomi/mimo-v2.5-pro-crof", "Mimo"),
            ("stealth/ox-alpha", "Alpha"),
            ("Baichuan4-Air", "Baichuan"),
            ("rekaai/reka-edge", "Reka"),
            ("allenai/olmo-3-32b-think", "Olmo"),
            ("codex-mini", "GPT"),
            ("lucidquery-nexus-coder", "Lucid"),
            ("hunyuan-turbos-20250226", "Hunyuan"),
            ("step-r1-v-mini", "Step"),
            ("yi-lightning", "Yi"),
            ("cohere/north-mini-code", "Cohere"),
            ("kimi-k2.7-code", "Kimi"),
            ("qwen/qwen3.5-35b-a3b", "Qwen"),
            ("grok/grok-4.3", "Grok"),
            ("mistralai/voxtral-small-24b-2507", "Mistral"),
            ("inclusionai/ring-2.6-1t", "Ring"),
            ("inclusionai/ling-3.0-flash", "Ling"),
            ("kwaipilot/kat-coder-pro-v2.5", "Kat"),
            ("tencent/hy-mt2-30b-a3b", "Hunyuan"),
            ("arcee-ai/virtuoso-large", "Arcee"),
        ];
        for (id, expected) in cases {
            assert_eq!(from_id(id), *expected, "model id {id}");
        }
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
