//! Cost computation from usage + resolved pricing.
//!
//! Conventions (see also `core::stats::build_document`):
//! - `usage.input` includes cached input; billed plain input is
//!   `input - cache_read` (never negative);
//! - reasoning tokens are reported separately and are **not** billed again:
//!   they are either already inside `output` (Anthropic thinking) or a
//!   detail of `output` (OpenAI reasoning_tokens);
//! - missing prices ⇒ all-null block plus warnings, never guessed numbers.

use lmhub_core::stats::{round, PricingBlock};
use lmhub_core::{PricingContext, Usage};

pub struct PricingOutcome {
    pub block: PricingBlock,
    pub warnings: Vec<String>,
}

pub fn compute(ctx: Option<&PricingContext>, usage: &Usage) -> PricingOutcome {
    let Some(ctx) = ctx else {
        return PricingOutcome {
            block: PricingBlock::unavailable(),
            warnings: vec![
                "no price found for this provider/model route in Models.dev; cost recorded as null"
                    .to_string(),
            ],
        };
    };

    let p = &ctx.pricing;
    let cache_read = usage.cache_read_tokens.unwrap_or(0);
    let cache_write = usage.cache_write_tokens.unwrap_or(0);

    let plain_input = usage
        .input_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);

    let per_million = |tokens: u64, price: f64| round(tokens as f64 / 1_000_000.0 * price, 8);

    let input_usd = per_million(plain_input, p.input_per_million_usd);
    let output_usd = per_million(usage.output_tokens, p.output_per_million_usd);
    let cache_read_usd = p
        .cache_read_per_million_usd
        .map(|price| per_million(cache_read, price));
    // Cache write has its own price only on some providers (e.g. Anthropic).
    let mut warnings = Vec::new();
    let cache_write_usd = match p.cache_write_per_million_usd {
        Some(price) => Some(per_million(cache_write, price)),
        None if cache_write > 0 => {
            warnings.push(format!(
                "cache-write price unknown for this route but {cache_write} cache-write tokens were used; cacheWriteUsd recorded as null"
            ));
            None
        }
        // No writes and no price → null (matches schema example; nothing was spent).
        None => None,
    };

    // Total: null whenever a used component has no known price.
    let cache_read_effective = if cache_read > 0 {
        cache_read_usd
    } else {
        Some(0.0)
    };
    let total_usd = match (
        Some(input_usd),
        Some(output_usd),
        cache_read_effective,
        cache_write_usd,
    ) {
        (Some(i), Some(o), Some(cr), Some(cw)) => Some(round(i + o + cr + cw, 8)),
        _ => None,
    };

    let block = PricingBlock {
        source: Some(ctx.source.clone()),
        fetched_at: ctx.fetched_at.clone(),
        snapshot_version: ctx.snapshot_version.clone(),
        input_per_million_tokens_usd: Some(p.input_per_million_usd),
        output_per_million_tokens_usd: Some(p.output_per_million_usd),
        cache_read_per_million_tokens_usd: p.cache_read_per_million_usd,
        cache_write_per_million_tokens_usd: p.cache_write_per_million_usd,
        input_usd: Some(input_usd),
        output_usd: Some(output_usd),
        cache_read_usd,
        cache_write_usd,
        total_usd,
    };

    PricingOutcome { block, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmhub_core::ModelPricing;

    fn ctx(
        input: f64,
        output: f64,
        cache_read: Option<f64>,
        cache_write: Option<f64>,
    ) -> PricingContext {
        PricingContext {
            pricing: ModelPricing {
                input_per_million_usd: input,
                output_per_million_usd: output,
                cache_read_per_million_usd: cache_read,
                cache_write_per_million_usd: cache_write,
            },
            source: "models.dev".into(),
            fetched_at: Some("2026-08-23T11:59:00Z".into()),
            snapshot_version: Some("abc123".into()),
        }
    }

    #[test]
    fn spec_example_math() {
        // prices 3 / 15 / 0.3 read; usage 4218 in (1800 cached), 8930 out
        let c = ctx(3.0, 15.0, Some(0.3), Some(3.75));
        let usage = Usage {
            input_tokens: 4218,
            output_tokens: 8930,
            reasoning_tokens: None,
            cache_read_tokens: Some(1800),
            cache_write_tokens: None,
        };
        let out = compute(Some(&c), &usage);
        let b = &out.block;
        assert_eq!(b.input_usd, Some(round((4218.0 - 1800.0) / 1e6 * 3.0, 8)));
        assert_eq!(b.output_usd, Some(round(8930.0 / 1e6 * 15.0, 8)));
        assert_eq!(b.cache_read_usd, Some(round(1800.0 / 1e6 * 0.3, 8)));
        assert!(b.total_usd.unwrap() > 0.13 && b.total_usd.unwrap() < 0.16);
    }

    #[test]
    fn missing_context_yields_null_block_and_warning() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        let out = compute(None, &usage);
        assert!(out.block.total_usd.is_none());
        assert_eq!(out.block.input_usd, None);
        assert!(!out.warnings.is_empty());
    }

    #[test]
    fn cache_write_without_price_stays_null_not_zero() {
        let c = ctx(1.0, 2.0, Some(0.1), None);
        let usage = Usage {
            input_tokens: 1005,
            output_tokens: 10,
            cache_read_tokens: None,
            cache_write_tokens: Some(1000),
            reasoning_tokens: None,
        };
        let out = compute(Some(&c), &usage);
        assert_eq!(out.block.cache_write_usd, None);
        assert_eq!(out.block.total_usd, None);
        assert!(!out.warnings.is_empty());
    }
}
