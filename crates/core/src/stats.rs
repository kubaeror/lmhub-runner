use crate::usage::Usage;
use serde::Serialize;
use std::time::Duration;

/// Terminal state of a run. `statistics.json` is written for every variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Completed,
    Error,
    Timeout,
    Cancelled,
    LimitExceeded,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::LimitExceeded => "limit_exceeded",
        }
    }
}

/// Round to 6 decimal places for stable cost reporting.
pub fn round(value: f64, places: u32) -> f64 {
    let factor = 10f64.powi(places as i32);
    (value * factor).round() / factor
}

/// `num / den`, `None` when the denominator is zero. No fabricated zeros.
pub fn ratio(num: f64, den: f64) -> Option<f64> {
    if den == 0.0 || !den.is_finite() {
        None
    } else {
        Some(round(num / den, 4))
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunMetrics {
    pub usage: Usage,
    pub llm_requests: u64,
    pub llm_duration_ms_total: u64,
    pub llm_duration_ms_max: u64,
    pub tool_calls_total: u64,
    pub tool_calls_successful: u64,
    pub tool_calls_failed: u64,
    pub errors_count: u32,
    pub warnings_count: u32,
    pub cache_enabled: bool,
    pub provider_cache_supported: bool,
    pub provider_cache_used: Option<bool>,
}

impl RunMetrics {
    pub fn new(cache_enabled: bool, provider_cache_supported: bool) -> Self {
        Self {
            cache_enabled,
            provider_cache_supported,
            ..Default::default()
        }
    }

    pub fn record_usage(&mut self, delta: &Usage) {
        self.usage.add(delta);
        let used =
            delta.cache_read_tokens.unwrap_or(0) > 0 || delta.cache_write_tokens.unwrap_or(0) > 0;
        if used {
            self.provider_cache_used = Some(true);
        }
    }

    pub fn note_request_without_cache(&mut self) {
        if self.provider_cache_used.is_none() {
            self.provider_cache_used = Some(false);
        }
    }

    pub fn record_llm_duration(&mut self, duration: Duration) {
        let ms = duration.as_millis() as u64;
        self.llm_requests += 1;
        self.llm_duration_ms_total += ms;
        self.llm_duration_ms_max = self.llm_duration_ms_max.max(ms);
    }

    pub fn record_tool_outcome(&mut self, success: bool) {
        self.tool_calls_total += 1;
        if success {
            self.tool_calls_successful += 1;
        } else {
            self.tool_calls_failed += 1;
        }
    }
}

/// Unique id for one run; also used (first 8 chars) in the run directory
/// name so statistics.json is traceable to its artifacts on disk.
pub fn gen_run_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokensBlock {
    pub input: u64,
    pub output: u64,
    pub reasoning: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub total: u64,
    pub cache_hit_ratio: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PerformanceBlock {
    pub tokens_per_second: Option<f64>,
    pub turns: u64,
    pub llm_requests: u64,
    pub avg_llm_request_ms: Option<u64>,
    pub max_llm_request_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallsBlock {
    pub total: u64,
    pub successful: u64,
    pub failed: u64,
    pub success_ratio: Option<f64>,
    pub failure_ratio: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheBlock {
    pub enabled: bool,
    pub provider_cache_supported: bool,
    pub provider_cache_used: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingBlock {
    pub source: Option<String>,
    pub fetched_at: Option<String>,
    pub snapshot_version: Option<String>,
    pub input_per_million_tokens_usd: Option<f64>,
    pub output_per_million_tokens_usd: Option<f64>,
    pub cache_read_per_million_tokens_usd: Option<f64>,
    pub cache_write_per_million_tokens_usd: Option<f64>,
    pub input_usd: Option<f64>,
    pub output_usd: Option<f64>,
    pub cache_read_usd: Option<f64>,
    pub cache_write_usd: Option<f64>,
    pub total_usd: Option<f64>,
}

impl PricingBlock {
    /// All-null block for runs without a known price. Never guessed prices.
    pub fn unavailable() -> Self {
        Self {
            source: None,
            fetched_at: None,
            snapshot_version: None,
            input_per_million_tokens_usd: None,
            output_per_million_tokens_usd: None,
            cache_read_per_million_tokens_usd: None,
            cache_write_per_million_tokens_usd: None,
            input_usd: None,
            output_usd: None,
            cache_read_usd: None,
            cache_write_usd: None,
            total_usd: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorsBlock {
    pub count: u32,
    pub log_path: String,
}

/// The exact document persisted as `statistics.json`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatisticsDocument {
    pub status: RunStatus,
    pub run_id: String,
    pub provider: String,
    pub provider_type: String,
    pub family: String,
    pub model: String,
    pub reasoning: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub tokens: TokensBlock,
    pub performance: PerformanceBlock,
    pub tool_calls: ToolCallsBlock,
    pub cache: CacheBlock,
    pub pricing: PricingBlock,
    pub errors: ErrorsBlock,
    pub warnings_count: u32,
}

/// Assemble the final document from accumulated metrics.
///
/// Cost convention (matches the schema example):
/// - `input` includes cache-read tokens;
///   `cache_hit_ratio = cache_read / input`;
/// - billed non-cached input = `input - cache_read`;
/// - reasoning tokens are reported separately and are NOT billed again —
///   providers either include them in `output` (Anthropic) or report them
///   as details of `output` (OpenAI).
#[allow(clippy::too_many_arguments)]
pub fn build_document(
    status: RunStatus,
    run_id: &str,
    identity: &RunIdentity,
    metrics: &RunMetrics,
    wall_duration: Duration,
    pricing: PricingBlock,
    errors_log_path: &str,
    finished_at: String,
) -> StatisticsDocument {
    let u = &metrics.usage;
    let cache_read = u.cache_read_tokens.unwrap_or(0);
    let cache_hit_ratio = ratio(cache_read as f64, u.input_tokens as f64);

    let generation_secs = metrics.llm_duration_ms_total as f64 / 1000.0;
    let tps = if generation_secs > 0.0 {
        Some(round(u.output_tokens as f64 / generation_secs, 2))
    } else {
        None
    };

    StatisticsDocument {
        status,
        run_id: run_id.to_string(),
        provider: identity.provider.to_string(),
        provider_type: identity.provider_type.to_string(),
        family: identity.family.to_string(),
        model: identity.model.to_string(),
        reasoning: identity.reasoning.to_string(),
        started_at: identity.started_at.clone(),
        finished_at: Some(finished_at),
        duration_ms: Some(wall_duration.as_millis() as u64),
        tokens: TokensBlock {
            input: u.input_tokens,
            output: u.output_tokens,
            reasoning: u.reasoning_tokens,
            cache_read: u.cache_read_tokens,
            cache_write: u.cache_write_tokens,
            total: u.total(),
            cache_hit_ratio,
        },
        performance: PerformanceBlock {
            tokens_per_second: tps,
            turns: metrics.llm_requests,
            llm_requests: metrics.llm_requests,
            avg_llm_request_ms: metrics
                .llm_duration_ms_total
                .checked_div(metrics.llm_requests),
            max_llm_request_ms: if metrics.llm_requests > 0 {
                Some(metrics.llm_duration_ms_max)
            } else {
                None
            },
        },
        tool_calls: ToolCallsBlock {
            total: metrics.tool_calls_total,
            successful: metrics.tool_calls_successful,
            failed: metrics.tool_calls_failed,
            success_ratio: ratio(
                metrics.tool_calls_successful as f64,
                metrics.tool_calls_total as f64,
            ),
            failure_ratio: ratio(
                metrics.tool_calls_failed as f64,
                metrics.tool_calls_total as f64,
            ),
        },
        cache: CacheBlock {
            enabled: metrics.cache_enabled,
            provider_cache_supported: metrics.provider_cache_supported,
            provider_cache_used: metrics.provider_cache_used,
        },
        pricing,
        errors: ErrorsBlock {
            count: metrics.errors_count,
            log_path: errors_log_path.to_string(),
        },
        warnings_count: metrics.warnings_count,
    }
}

/// Static identity of the run copied into statistics.json.
#[derive(Debug, Clone)]
pub struct RunIdentity {
    pub provider: String,
    pub provider_type: String,
    pub family: String,
    pub model: String,
    pub reasoning: String,
    pub started_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratios_null_on_zero_denominator() {
        assert_eq!(ratio(5.0, 0.0), None);
        assert_eq!(ratio(0.0, 0.0), None);
        assert_eq!(ratio(1800.0, 4218.0), Some(0.4267));
        assert_eq!(ratio(16.0, 18.0), Some(0.8889));
    }

    #[test]
    fn rounding_is_stable() {
        assert_eq!(round(0.14714444, 6), 0.147144);
        assert_eq!(round(1.23456789, 6), 1.234568);
    }

    #[test]
    fn metrics_track_tools_and_cache() {
        let mut m = RunMetrics::new(true, true);
        m.record_tool_outcome(true);
        m.record_tool_outcome(true);
        m.record_tool_outcome(false);
        assert_eq!(m.tool_calls_total, 3);
        assert_eq!(m.tool_calls_successful, 2);
        m.record_usage(&Usage {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: Some(40),
            ..Default::default()
        });
        assert_eq!(m.provider_cache_used, Some(true));
    }
}
