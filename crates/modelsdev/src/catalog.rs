use lmhub_core::ReasoningLevel;
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
    /// Declared reasoning behavior — the same `reasoning_options` field
    /// opencode consumes: effort levels, a plain toggle, or a thinking
    /// budget. `None` (absent) and `[]` (empty) mean "no declaration".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_options: Vec<ReasoningOption>,
}

/// One entry of models.dev `reasoning_options`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    /// Explicit effort levels; `null` values mean "none"/off.
    Effort { values: Vec<Option<String>> },
    /// Plain on/off reasoning switch.
    Toggle,
    /// Thinking-budget style reasoning (e.g. Anthropic).
    BudgetTokens {
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
    },
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

impl ModelEntry {
    /// Supported reasoning levels derived from `reasoning_options`
    /// (mirrors opencode's `reasoningVariants`):
    /// - `effort` values map 1:1 (`null` → off), e.g. `["high","max"]`;
    /// - `toggle` → `[off, high]`;
    /// - `budget_tokens` → `[high, max]` (off only when a toggle is present);
    /// - absent or empty declaration → `None` (unknown, all levels offered).
    pub fn reasoning_levels(&self) -> Option<Vec<ReasoningLevel>> {
        let options = self.reasoning_options.as_slice();
        if options.is_empty() {
            return None;
        }
        let effort = options.iter().find_map(|o| match o {
            ReasoningOption::Effort { values } => Some(values),
            _ => None,
        });
        let has_toggle = options.iter().any(|o| matches!(o, ReasoningOption::Toggle));
        let has_budget = options
            .iter()
            .any(|o| matches!(o, ReasoningOption::BudgetTokens { .. }));

        if let Some(values) = effort {
            let mut levels: Vec<ReasoningLevel> = Vec::new();
            for value in values {
                let level = match value.as_deref() {
                    None => ReasoningLevel::Off,
                    Some(s) => match s.to_ascii_lowercase().as_str() {
                        "none" | "off" => ReasoningLevel::Off,
                        "minimal" => ReasoningLevel::Minimal,
                        "low" => ReasoningLevel::Low,
                        "medium" => ReasoningLevel::Medium,
                        "high" => ReasoningLevel::High,
                        "xhigh" => ReasoningLevel::XHigh,
                        "max" => ReasoningLevel::Max,
                        _ => continue,
                    },
                };
                if !levels.contains(&level) {
                    levels.push(level);
                }
            }
            levels.sort();
            return Some(levels);
        }

        let mut levels = Vec::new();
        if has_toggle {
            // Plain switch → off/high (mirrors opencode's reasoningToggle).
            levels.push(ReasoningLevel::Off);
            levels.push(ReasoningLevel::High);
        }
        if has_budget {
            levels.push(ReasoningLevel::High);
            levels.push(ReasoningLevel::Max);
        }
        if levels.is_empty() {
            // Unrecognized option shape — treat as unknown.
            return None;
        }
        levels.sort();
        levels.dedup();
        Some(levels)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(reasoning_options: &str) -> ModelEntry {
        let json = format!(
            r#"{{"id": "m", "name": "M", "reasoning": true, "reasoning_options": {reasoning_options}}}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn effort_values_map_to_levels_in_order() {
        let e = entry(r#"[{"type": "effort", "values": ["high", "max"]}]"#);
        assert_eq!(
            e.reasoning_levels(),
            Some(vec![ReasoningLevel::High, ReasoningLevel::Max])
        );
    }

    #[test]
    fn null_effort_value_means_off() {
        let e = entry(
            r#"[{"type": "effort", "values": [null, "low", "medium", "high", "xhigh", "max"]}]"#,
        );
        assert_eq!(
            e.reasoning_levels(),
            Some(vec![
                ReasoningLevel::Off,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ])
        );
    }

    #[test]
    fn toggle_maps_to_off_and_high() {
        let e = entry(r#"[{"type": "toggle"}]"#);
        assert_eq!(
            e.reasoning_levels(),
            Some(vec![ReasoningLevel::Off, ReasoningLevel::High])
        );
    }

    #[test]
    fn budget_tokens_maps_to_high_and_max() {
        let e = entry(r#"[{"type": "budget_tokens", "min": 1024, "max": 32000}]"#);
        assert_eq!(
            e.reasoning_levels(),
            Some(vec![ReasoningLevel::High, ReasoningLevel::Max])
        );
    }

    #[test]
    fn missing_or_empty_options_mean_unknown() {
        let missing: ModelEntry = serde_json::from_str(r#"{"id": "m", "name": "M"}"#).unwrap();
        assert_eq!(missing.reasoning_levels(), None);
        let empty = entry("[]");
        assert_eq!(empty.reasoning_levels(), None);
    }
}
