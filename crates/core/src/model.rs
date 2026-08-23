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
