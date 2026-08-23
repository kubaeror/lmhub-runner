use serde::Deserialize;
use std::collections::BTreeMap;

/// Tolerant Models.dev catalog structs: unknown fields are ignored and
/// optional fields default to `None`, so schema drift never breaks the runner.

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    #[serde(flatten)]
    pub providers: BTreeMap<String, ProviderEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Environment variable names the upstream provider reads its key from.
    #[serde(default)]
    pub env: Vec<String>,
    /// Documented API base URL (may be absent for well-known providers).
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub doc: Option<String>,
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub attachment: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub tool_call: Option<bool>,
    #[serde(default)]
    pub structured_output: Option<bool>,
    #[serde(default)]
    pub temperature: Option<bool>,
    #[serde(default)]
    pub knowledge: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub last_updated: Option<String>,
    #[serde(default)]
    pub open_weights: Option<bool>,
    #[serde(default)]
    pub limit: Option<LimitBlock>,
    #[serde(default)]
    pub cost: Option<CostBlock>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct LimitBlock {
    pub context: Option<u64>,
    pub output: Option<u64>,
}

/// Prices in USD per million tokens.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct CostBlock {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

impl Catalog {
    pub fn parse(raw: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(raw)
    }

    pub fn get(&self, provider_id: &str) -> Option<&ProviderEntry> {
        self.providers.get(provider_id)
    }

    pub fn model(&self, provider_id: &str, model_id: &str) -> Option<&ModelEntry> {
        self.providers.get(provider_id)?.models.get(model_id)
    }

    /// Find a model across all providers. Providers are scanned in stable
    /// (alphabetical) order; `preferred` is tried first when given.
    pub fn find_model_anywhere(
        &self,
        model_id: &str,
        preferred: Option<&str>,
    ) -> Option<(String, &ModelEntry)> {
        if let Some(pref) = preferred {
            if let Some(entry) = self.model(pref, model_id) {
                return Some((pref.to_string(), entry));
            }
        }
        for (pid, prov) in self.providers.iter() {
            if let Some(entry) = prov.models.get(model_id) {
                return Some((pid.clone(), entry));
            }
        }
        // Second pass on the last path segment ("vendor/model" style ids).
        let tail = model_id.rsplit(['/', ':']).next().unwrap_or(model_id);
        if tail != model_id {
            return self.find_model_anywhere(tail, preferred);
        }
        None
    }
}
