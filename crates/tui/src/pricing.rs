//! Live cost estimation — same math as `agent::pricing::compute` so the
//! on-screen number matches the final statistics.json.
//!
//! Convention (see crates/core/src/stats.rs): `input` includes cache-read
//! tokens; cache-write tokens are billed at their own rate and excluded from
//! plain input billing; reasoning tokens are never billed twice.

use lmhub_core::{ModelPricing, Usage};

/// Estimated USD for a run's accumulated usage. `None` = no price known
/// (cost must be reported as null, never guessed).
pub fn estimate_cost(pricing: &ModelPricing, usage: &Usage) -> f64 {
    let cr = usage.cache_read_tokens.unwrap_or(0) as f64;
    let cw = usage.cache_write_tokens.unwrap_or(0) as f64;
    let plain = (usage.input_tokens as f64 - cr - cw).max(0.0);
    plain / 1e6 * pricing.input_per_million_usd
        + usage.output_tokens as f64 / 1e6 * pricing.output_per_million_usd
        + cr / 1e6 * pricing.cache_read_per_million_usd.unwrap_or(0.0)
        + cw / 1e6 * pricing.cache_write_per_million_usd.unwrap_or(0.0)
}

/// Cache hit ratio, `None` when there is no input to speak of.
pub fn cache_hit_ratio(usage: &Usage) -> Option<f64> {
    if usage.input_tokens == 0 {
        None
    } else {
        Some(usage.cache_read_tokens.unwrap_or(0) as f64 / usage.input_tokens as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_agent_pricing_convention() {
        // 100 in total, 40 cached-read, 10 cached-write → 50 plain input.
        let u = Usage {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: Some(40),
            cache_write_tokens: Some(10),
            ..Default::default()
        };
        let p = ModelPricing {
            input_per_million_usd: 3.0,
            output_per_million_usd: 15.0,
            cache_read_per_million_usd: Some(0.3),
            cache_write_per_million_usd: Some(3.75),
        };
        let expect = 50.0 / 1e6 * 3.0 + 10.0 / 1e6 * 15.0 + 40.0 / 1e6 * 0.3 + 10.0 / 1e6 * 3.75;
        assert!((estimate_cost(&p, &u) - expect).abs() < 1e-12);
    }

    #[test]
    fn no_fabricated_cache_cost() {
        let u = Usage {
            input_tokens: 10,
            output_tokens: 1,
            ..Default::default()
        };
        let p = ModelPricing {
            input_per_million_usd: 1.0,
            output_per_million_usd: 2.0,
            cache_read_per_million_usd: None,
            cache_write_per_million_usd: None,
        };
        assert!((estimate_cost(&p, &u) - 12.0 / 1e6).abs() < 1e-12);
    }

    #[test]
    fn hit_ratio_null_without_input() {
        assert_eq!(cache_hit_ratio(&Usage::default()), None);
        let u = Usage {
            input_tokens: 100,
            cache_read_tokens: Some(40),
            ..Default::default()
        };
        assert!((cache_hit_ratio(&u).unwrap() - 0.4).abs() < 1e-9);
    }
}
