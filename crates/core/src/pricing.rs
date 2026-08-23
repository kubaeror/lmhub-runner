use serde::{Deserialize, Serialize};

/// Price of a specific model at a specific provider, USD per million tokens.
///
/// Never guessed: constructed only when a real catalog entry provides it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
    pub cache_read_per_million_usd: Option<f64>,
    pub cache_write_per_million_usd: Option<f64>,
}

/// A resolved price plus its provenance, recorded verbatim in statistics.json.
#[derive(Debug, Clone)]
pub struct PricingContext {
    pub pricing: ModelPricing,
    /// Catalog source, e.g. `"models.dev"`.
    pub source: String,
    /// When the catalog was fetched (RFC3339).
    pub fetched_at: Option<String>,
    /// Content-hash of the catalog snapshot.
    pub snapshot_version: Option<String>,
}

impl ModelPricing {
    /// Returns `None` when the entry lacks base input/output prices —
    /// the runner must not invent a price.
    pub fn from_parts(
        input: Option<f64>,
        output: Option<f64>,
        cache_read: Option<f64>,
        cache_write: Option<f64>,
    ) -> Option<Self> {
        match (input, output) {
            (Some(i), Some(o)) if i.is_finite() && o.is_finite() && i >= 0.0 && o >= 0.0 => {
                Some(Self {
                    input_per_million_usd: i,
                    output_per_million_usd: o,
                    cache_read_per_million_usd: cache_read.filter(|v| v.is_finite() && *v >= 0.0),
                    cache_write_per_million_usd: cache_write.filter(|v| v.is_finite() && *v >= 0.0),
                })
            }
            _ => None,
        }
    }
}
