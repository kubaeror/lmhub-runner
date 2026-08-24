use serde::{Deserialize, Serialize};

use crate::Usage;

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

impl ModelPricing {
    /// Live cost estimate for `usage` under these prices.
    ///
    /// This is the fast on-screen number; the authoritative figure recorded
    /// in `statistics.json` is `agent::pricing::compute`, which returns
    /// `null` (with a warning) when any used component lacks a price. Here
    /// cache tokens bill at 0 when their rate is unknown — good enough for
    /// an `est … USD` counter, never for the audit trail.
    pub fn estimate_cost(&self, usage: &Usage) -> f64 {
        let cr = usage.cache_read_tokens.unwrap_or(0) as f64;
        let cw = usage.cache_write_tokens.unwrap_or(0) as f64;
        let plain = (usage.input_tokens as f64 - cr - cw).max(0.0);
        plain / 1e6 * self.input_per_million_usd
            + usage.output_tokens as f64 / 1e6 * self.output_per_million_usd
            + cr / 1e6 * self.cache_read_per_million_usd.unwrap_or(0.0)
            + cw / 1e6 * self.cache_write_per_million_usd.unwrap_or(0.0)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing() -> ModelPricing {
        ModelPricing {
            input_per_million_usd: 3.0,
            output_per_million_usd: 15.0,
            cache_read_per_million_usd: Some(0.3),
            cache_write_per_million_usd: Some(3.75),
        }
    }

    #[test]
    fn estimate_matches_billed_input_convention() {
        // 100 in total, 40 cached-read, 10 cached-write → 50 plain input.
        let u = Usage {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: Some(40),
            cache_write_tokens: Some(10),
            ..Default::default()
        };
        let expect = 50.0 / 1e6 * 3.0 + 10.0 / 1e6 * 15.0 + 40.0 / 1e6 * 0.3 + 10.0 / 1e6 * 3.75;
        assert!((pricing().estimate_cost(&u) - expect).abs() < 1e-12);
    }

    #[test]
    fn estimate_without_cache_prices_bills_cache_at_zero() {
        let p = ModelPricing {
            input_per_million_usd: 1.0,
            output_per_million_usd: 2.0,
            cache_read_per_million_usd: None,
            cache_write_per_million_usd: None,
        };
        let u = Usage {
            input_tokens: 10,
            output_tokens: 1,
            cache_read_tokens: Some(5),
            ..Default::default()
        };
        // 5 plain input + 1 output; the 5 cache-read tokens bill at 0 here
        // (estimate only — the authoritative doc would record null).
        assert!((p.estimate_cost(&u) - 7.0 / 1e6).abs() < 1e-12);
    }
}
