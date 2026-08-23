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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub tool_call: bool,
    pub reasoning: bool,
    pub prompt_caching: bool,
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

    pub fn is_usable(&self) -> bool {
        !self.models.is_empty()
    }
}
