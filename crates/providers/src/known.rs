//! Bundled catalog of known providers (snapshot of models.dev — the same
//! source opencode uses). Regenerate with `cargo run -p xtask -- gen-providers`.

use serde::Deserialize;

pub const SNAPSHOT_JSON: &str = include_str!("known/known_providers.json");

/// Wire protocol / adapter a provider routes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolKind {
    /// Plain OpenAI Chat Completions wire at the given base URL.
    OpenAiCompat,
    /// Anthropic Messages wire.
    AnthropicCompat,
    /// Azure OpenAI (deployment URLs + `api-key` header).
    Azure,
    /// Google Gemini native `generateContent` (Generative Language API).
    GeminiNative,
    /// Vertex AI hosting Gemini models (project/location URL, OAuth bearer).
    VertexGemini,
    /// Vertex AI hosting Claude models (Anthropic wire on Vertex endpoints).
    VertexAnthropic,
    /// Amazon Bedrock Converse API (bearer token or SigV4).
    Bedrock,
    /// Cohere `/v2/chat` native schema.
    Cohere,
    /// GitLab Duo (ai-gateway URL, PAT).
    GitLabDuo,
    /// GitHub Copilot (device-flow OAuth + api.githubcopilot.com).
    Copilot,
    /// OpenAI-compatible wire behind an IBM IAM token pre-auth step.
    OpenAiCompatIbmIam,
    /// OpenAI-compatible wire behind an OAuth client-credentials pre-auth.
    OpenAiCompatOauthCc,
}

impl ProtocolKind {
    pub fn provider_type(&self) -> &'static str {
        match self {
            Self::OpenAiCompat => "openai-compatible",
            Self::AnthropicCompat => "anthropic-compatible",
            Self::Azure => "azure-openai",
            Self::GeminiNative => "google-gemini",
            Self::VertexGemini => "google-vertex-gemini",
            Self::VertexAnthropic => "google-vertex-anthropic",
            Self::Bedrock => "aws-bedrock",
            Self::Cohere => "cohere",
            Self::GitLabDuo => "gitlab-duo",
            Self::Copilot => "github-copilot",
            Self::OpenAiCompatIbmIam => "watsonx-iam",
            Self::OpenAiCompatOauthCc => "oauth-client-credentials",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct KnownEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedKnown {
    pub entry: KnownEntry,
    pub protocol: ProtocolKind,
    /// Effective base URL (catalog value, well-known fallback, or dynamic
    /// placeholder that the adapter resolves from env at call time).
    pub base_url: Option<String>,
    /// False for local runtimes (ollama/lmstudio/…) — no key needed.
    pub requires_key: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct KnownCatalog {
    #[serde(default)]
    pub snapshot_version: String,
    #[serde(default)]
    pub providers: Vec<KnownEntry>,
}

impl KnownCatalog {
    pub fn load() -> Self {
        serde_json::from_str(SNAPSHOT_JSON).unwrap_or_default()
    }

    /// Resolve protocol + effective settings for every entry.
    pub fn resolved(&self) -> Vec<ResolvedKnown> {
        self.providers.iter().map(resolve_entry).collect()
    }
}

fn npm_to_protocol(npm: &str) -> Option<ProtocolKind> {
    Some(match npm {
        "@ai-sdk/openai-compatible" | "@ai-sdk/openai" => ProtocolKind::OpenAiCompat,
        "@ai-sdk/anthropic" => ProtocolKind::AnthropicCompat,
        "@ai-sdk/azure" => ProtocolKind::Azure,
        "@ai-sdk/google" => ProtocolKind::GeminiNative,
        "@ai-sdk/google-vertex" => ProtocolKind::VertexGemini,
        "@ai-sdk/google-vertex/anthropic" => ProtocolKind::VertexAnthropic,
        "@ai-sdk/amazon-bedrock" => ProtocolKind::Bedrock,
        "@ai-sdk/cohere" => ProtocolKind::Cohere,
        "gitlab-ai-provider" => ProtocolKind::GitLabDuo,
        // Custom SDK packages whose REST surface is still OpenAI-shaped:
        "@openrouter/ai-sdk-provider"
        | "@ai-sdk/groq"
        | "@ai-sdk/xai"
        | "@ai-sdk/perplexity"
        | "@ai-sdk/mistral"
        | "@ai-sdk/togetherai"
        | "@ai-sdk/deepinfra"
        | "@ai-sdk/cerebras"
        | "ai-gateway-provider"
        | "venice-ai-sdk-provider"
        | "aihubmix-ai-sdk-provider"
        | "@saladtechnologies-oss/ai-sdk-provider"
        | "merge-gateway-ai-sdk-provider"
        | "@qvac/ai-sdk-provider" => ProtocolKind::OpenAiCompat,
        "watsonx-ai-provider" => ProtocolKind::OpenAiCompatIbmIam,
        "@jerome-benoit/sap-ai-provider-v2" => ProtocolKind::OpenAiCompatOauthCc,
        "@ai-sdk/vercel" => ProtocolKind::OpenAiCompat,
        _ => return None,
    })
}

/// Provider-id specific overrides applied after npm mapping.
fn id_override(id: &str) -> Option<ProtocolKind> {
    match id {
        "github-copilot" => Some(ProtocolKind::Copilot),
        "gitlab" => Some(ProtocolKind::GitLabDuo),
        "watsonx" => Some(ProtocolKind::OpenAiCompatIbmIam),
        "sap-ai-core" => Some(ProtocolKind::OpenAiCompatOauthCc),
        "cohere" => Some(ProtocolKind::Cohere),
        _ => None,
    }
}

/// Well-known base URLs for entries where models.dev has no `api` field
/// (mirrors what opencode/AI SDK hardcode). `None` = dynamic (adapter
/// resolves from env) or genuinely unknown (needs explicit TOML config).
fn fallback_url(id: &str) -> Option<&'static str> {
    const DYNAMIC: Option<&str> = None;
    match id {
        "cerebras" => Some("https://api.cerebras.ai/v1"),
        "groq" => Some("https://api.groq.com/openai/v1"),
        "mistral" => Some("https://api.mistral.ai/v1"),
        "perplexity" => Some("https://api.perplexity.ai"),
        "togetherai" => Some("https://api.together.xyz/v1"),
        "deepinfra" => Some("https://api.deepinfra.com/v1/openai"),
        "xai" => Some("https://api.x.ai/v1"),
        "venice" => Some("https://api.venice.ai/api/v1"),
        "v0" => Some("https://api.v0.dev/v1"),
        "vercel" => Some("https://ai-gateway.vercel.sh/v1"),
        "aihubmix" => Some("https://aihubmix.com/v1"),
        "salad-cloud" => Some("https://salad.com/api/inference"),
        "cohere" => Some("https://api.cohere.com"),
        "gitlab" => Some("https://ai-gateway.gitlab.com/v1"),

        // Dynamic — adapters build these from environment:
        "anthropic" => Some("https://api.anthropic.com/v1"), // builtin anyway
        "openai" => Some("https://api.openai.com/v1"),       // builtin anyway
        "azure" | "azure-cognitive-services" => DYNAMIC,
        "google" => Some("https://generativelanguage.googleapis.com/v1beta"),
        "google-vertex" | "google-vertex-anthropic" => DYNAMIC,
        "amazon-bedrock" => DYNAMIC,
        "cloudflare-ai-gateway" => DYNAMIC,
        "qvac" => Some("http://127.0.0.1:8090/v1"),
        "sap-ai-core" => DYNAMIC,
        _ => None,
    }
}

const LOCAL_IDS: &[&str] = &[
    "ollama",
    "lmstudio",
    "llamacpp",
    "atomic-chat",
    "vllm",
    "qvac",
    "localai",
];

pub fn resolve_entry(entry: &KnownEntry) -> ResolvedKnown {
    let protocol = id_override(&entry.id)
        .or_else(|| entry.npm.as_deref().and_then(npm_to_protocol))
        .unwrap_or(ProtocolKind::OpenAiCompat);

    let base_url = entry
        .api
        .clone()
        .or_else(|| fallback_url(&entry.id).map(|s| s.to_string()));

    let requires_key = if LOCAL_IDS.contains(&entry.id.as_str()) {
        false
    } else {
        !entry.env.is_empty() || !matches!(protocol, ProtocolKind::OpenAiCompat)
    };

    ResolvedKnown {
        entry: entry.clone(),
        protocol,
        base_url,
        requires_key,
    }
}

/// Every known provider resolved and ready for registry insertion.
pub fn all_resolved() -> Vec<ResolvedKnown> {
    KnownCatalog::load().resolved()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_loads_and_has_expected_size() {
        let cat = KnownCatalog::load();
        assert!(cat.providers.len() >= 150, "got {}", cat.providers.len());
        assert!(!cat.snapshot_version.is_empty());
    }

    #[test]
    fn maps_protocols_from_npm() {
        let e = |npm: Option<&str>| KnownEntry {
            id: "t".into(),
            name: "T".into(),
            npm: npm.map(String::from),
            api: Some("https://x/v1".into()),
            env: vec!["KEY".into()],
        };
        assert_eq!(
            resolve_entry(&e(Some("@ai-sdk/openai-compatible"))).protocol,
            ProtocolKind::OpenAiCompat
        );
        assert_eq!(
            resolve_entry(&e(Some("@ai-sdk/anthropic"))).protocol,
            ProtocolKind::AnthropicCompat
        );
        assert_eq!(
            resolve_entry(&e(Some("@ai-sdk/google"))).protocol,
            ProtocolKind::GeminiNative
        );
    }

    #[test]
    fn id_overrides_win_over_npm() {
        let copilot = KnownEntry {
            id: "github-copilot".into(),
            name: "GitHub Copilot".into(),
            npm: Some("@ai-sdk/openai-compatible".into()),
            api: Some("https://api.githubcopilot.com".into()),
            env: vec![],
        };
        assert_eq!(resolve_entry(&copilot).protocol, ProtocolKind::Copilot);
    }

    #[test]
    fn local_runtimes_need_no_key() {
        let ollama = KnownEntry {
            id: "ollama".into(),
            name: "Ollama".into(),
            npm: Some("@ai-sdk/openai-compatible".into()),
            api: Some("http://localhost:11434/v1".into()),
            env: vec![],
        };
        assert!(!resolve_entry(&ollama).requires_key);
    }

    #[test]
    fn well_known_fallbacks_present() {
        let groq = KnownEntry {
            id: "groq".into(),
            name: "Groq".into(),
            npm: Some("@ai-sdk/groq".into()),
            api: None,
            env: vec!["GROQ_API_KEY".into()],
        };
        let r = resolve_entry(&groq);
        assert_eq!(
            r.base_url.as_deref(),
            Some("https://api.groq.com/openai/v1")
        );
    }

    #[test]
    fn real_snapshot_spot_checks() {
        let cat = KnownCatalog::load();
        let find = |id: &str| cat.providers.iter().find(|p| p.id == id).unwrap().clone();
        assert_eq!(
            resolve_entry(&find("amazon-bedrock")).protocol,
            ProtocolKind::Bedrock
        );
        assert_eq!(
            resolve_entry(&find("google-vertex-anthropic")).protocol,
            ProtocolKind::VertexAnthropic
        );
        assert_eq!(
            resolve_entry(&find("watsonx")).protocol,
            ProtocolKind::OpenAiCompatIbmIam
        );
        // openrouter keeps its api url from the catalog
        assert!(find("openrouter").api.is_some());
    }

    /// Coverage audit: every snapshot entry must be routable — either it has
    /// a concrete URL (catalog or fallback) or its protocol resolves the URL
    /// dynamically from the environment.
    #[test]
    fn every_known_provider_is_routable() {
        const DYNAMIC_PROTOCOL_IDS: &[&str] = &[
            "azure",
            "azure-cognitive-services",
            "google-vertex",
            "google-vertex-anthropic",
            "amazon-bedrock",
            "cloudflare-ai-gateway",
            "sap-ai-core",
            "watsonx", // WATSONX_AI_URL with documented default region host
        ];
        let unroutable: Vec<String> = KnownCatalog::load()
            .resolved()
            .into_iter()
            .filter(|r| {
                r.base_url.is_none() && !DYNAMIC_PROTOCOL_IDS.contains(&r.entry.id.as_str())
            })
            .map(|r| r.entry.id)
            .collect();
        assert!(
            unroutable.is_empty(),
            "entries without URL/fallback/dynamic handling: {unroutable:?}"
        );
    }
}
