use crate::chat::ReasoningLevel;
use serde::{Deserialize, Serialize};

/// Where a model list came from. Shown verbatim in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelListSource {
    ProviderApi,
    ModelsDev,
    LocalConfig,
}

impl ModelListSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ProviderApi => "Provider API",
            Self::ModelsDev => "Models.dev",
            Self::LocalConfig => "Local provider config",
        }
    }
}

/// Feature flags for a single model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub tool_call: bool,
    pub reasoning: bool,
    pub prompt_caching: bool,
    /// Declared supported reasoning levels (models.dev `reasoning_options`).
    /// `None` = no declaration → all levels are offered; `Some(vec![])` is
    /// not produced (empty declarations are treated as unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<ReasoningLevel>>,
    /// Declared thinking-budget range in tokens (models.dev
    /// `reasoning_options.budget_tokens`). Adapters derive a concrete budget
    /// per level from this range; `None` falls back to their constants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_budget_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_budget_max: Option<u64>,
}

/// A model known to the runner (from any source).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub family: Option<String>,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    pub capabilities: Capabilities,
}

impl ModelInfo {
    pub fn bare(id: &str) -> Self {
        Self {
            id: id.to_string(),
            name: id.to_string(),
            ..Default::default()
        }
    }

    /// Merge metadata from Models.dev without losing locally-known values.
    pub fn merge_from(&mut self, other: &ModelInfo) {
        if self.name.is_empty() || self.name == self.id {
            self.name = other.name.clone();
        }
        if self.family.is_none() {
            self.family = other.family.clone();
        }
        if self.context_window.is_none() {
            self.context_window = other.context_window;
        }
        if self.max_output.is_none() {
            self.max_output = other.max_output;
        }
        // Capabilities: OR-merge so either source can enable a feature.
        self.capabilities.tool_call |= other.capabilities.tool_call;
        self.capabilities.reasoning |= other.capabilities.reasoning;
        self.capabilities.prompt_caching |= other.capabilities.prompt_caching;
        // Reasoning levels: prefer the narrower declaration — intersect when
        // both sources declare them.
        self.capabilities.reasoning_levels = match (
            &self.capabilities.reasoning_levels,
            &other.capabilities.reasoning_levels,
        ) {
            (Some(a), Some(b)) => Some(
                a.iter()
                    .copied()
                    .filter(|l| b.contains(l))
                    .collect::<Vec<_>>(),
            ),
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            (None, None) => None,
        };
        // Thinking-budget range: fill gaps, but keep the narrower max when
        // both sources declare one.
        self.capabilities.reasoning_budget_min = match (
            self.capabilities.reasoning_budget_min,
            other.capabilities.reasoning_budget_min,
        ) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, b) => b,
        };
        self.capabilities.reasoning_budget_max = match (
            self.capabilities.reasoning_budget_max,
            other.capabilities.reasoning_budget_max,
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, b) => b,
        };
    }
}

/// Result of model resolution with explicit provenance.
#[derive(Debug, Clone)]
pub struct ModelCatalog {
    pub models: Vec<ModelInfo>,
    pub source: Option<ModelListSource>,
    pub warnings: Vec<String>,
}

impl ModelCatalog {
    pub fn empty() -> Self {
        Self {
            models: Vec::new(),
            source: None,
            warnings: Vec::new(),
        }
    }
}
