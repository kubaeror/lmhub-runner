//! Provider registry and the model-resolution fallback chain:
//!
//! 1. **Provider API** (`/models`) when the upstream exposes it;
//! 2. **Models.dev** catalog (cached locally);
//! 3. **Local provider config** (custom `providers/*.toml` static models).
//!
//! The chosen source is reported in [`ModelCatalog::source`] and shown in the TUI.

use crate::known::{KnownCatalog, ProtocolKind};
use crate::routed::RoutedProvider;
use lmhub_core::{
    AuthStore, Capabilities, LocalModel, ModelCatalog, ModelInfo, ModelListSource, PricingContext,
    Provider,
};
use lmhub_modelsdev::catalog::ModelEntry;
use lmhub_modelsdev::{pricing_for, ModelsDevClient};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    items: Vec<Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new(items: Vec<Arc<dyn Provider>>) -> Self {
        Self { items }
    }

    pub fn all(&self) -> &[Arc<dyn Provider>] {
        &self.items
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.items.iter().find(|p| p.id() == id).cloned()
    }
}

/// GitLab Duo exposes a fixed trio of Claude-backed chat models.
const GITLAB_DUO_MODELS: &[(&str, &str)] = &[
    ("duo-chat-haiku-4-5", "Duo Chat Haiku 4.5"),
    ("duo-chat-sonnet-4-5", "Duo Chat Sonnet 4.5"),
    ("duo-chat-opus-4-5", "Duo Chat Opus 4.5"),
];

/// Build the full registry:
/// 1. built-in native adapters (openai, anthropic);
/// 2. every known catalog provider (models.dev snapshot) routed by protocol;
/// 3. user TOMLs from `providers/` (highest precedence per id).
pub fn build_registry(
    providers_dir: &std::path::Path,
    auth_store: Arc<Mutex<AuthStore>>,
) -> (ProviderRegistry, Vec<String>) {
    let mut items: Vec<Arc<dyn Provider>> = vec![
        crate::anthropic::NativeAnthropicProvider::standard(),
        crate::openai::NativeOpenAiProvider::standard(),
    ];
    let mut errors: Vec<String> = Vec::new();

    let known = KnownCatalog::load();
    for resolved in known.resolved() {
        // Built-ins already cover these ids with dedicated implementations.
        if resolved.entry.id == "anthropic" || resolved.entry.id == "openai" {
            continue;
        }
        if !resolved.requires_key
            && resolved.base_url.is_none()
            && matches!(resolved.protocol, ProtocolKind::OpenAiCompat)
        {
            // Local runtimes without any URL are unusable — skip silently.
            continue;
        }
        let mut local_models: Vec<LocalModel> = Vec::new();
        if resolved.protocol == ProtocolKind::GitLabDuo {
            // Duo chat models may be absent from the snapshot; seed them so
            // the picker is usable even without Models.dev.
            for (id, name) in GITLAB_DUO_MODELS {
                local_models.push(LocalModel {
                    id: (*id).to_string(),
                    name: Some((*name).to_string()),
                    family: Some("Claude".into()),
                    reasoning: true,
                    tool_call: true,
                    context_window: Some(200_000),
                    max_output: Some(8_192),
                    reasoning_levels: None,
                });
            }
        }
        let provider = RoutedProvider::from_parts(
            resolved.entry.id.clone(),
            resolved.entry.name.clone(),
            resolved.protocol,
            resolved.base_url.clone(),
            resolved.entry.env.clone(),
            resolved.entry.id.clone(),
            resolved.requires_key,
            Arc::clone(&auth_store),
            local_models,
        );
        items.push(provider);
    }

    let (mut custom, custom_errors) = crate::config::load_providers_from_dir(providers_dir);
    errors.extend(custom_errors);

    // Custom TOMLs win over known entries with the same id.
    let custom_ids: std::collections::HashSet<String> =
        custom.iter().map(|p| p.id().to_string()).collect();
    items.retain(|p| !custom_ids.contains(p.id()));
    items.append(&mut custom);

    items.sort_by_key(|p| p.display_name().to_ascii_lowercase());
    (ProviderRegistry { items }, errors)
}

fn local_to_info(lm: &LocalModel) -> ModelInfo {
    ModelInfo {
        id: lm.id.clone(),
        name: lm.name.clone().unwrap_or_else(|| lm.id.clone()),
        family: lm.family.clone(),
        context_window: lm.context_window,
        max_output: lm.max_output,
        capabilities: Capabilities {
            tool_call: lm.tool_call,
            reasoning: lm.reasoning,
            prompt_caching: false, // unknown until Models.dev says otherwise
            reasoning_levels: lm.parsed_reasoning_levels(),
        },
    }
}

pub fn entry_to_info(entry: &ModelEntry) -> ModelInfo {
    let limits = entry.limit.as_ref();
    ModelInfo {
        id: entry.id.clone(),
        name: if entry.name.is_empty() {
            entry.id.clone()
        } else {
            entry.name.clone()
        },
        family: entry.family.clone(),
        context_window: limits.and_then(|l| l.context),
        max_output: limits.and_then(|l| l.output),
        capabilities: Capabilities {
            tool_call: entry.tool_call.unwrap_or(false),
            reasoning: entry.reasoning.unwrap_or(false),
            // A cache_read price implies caching support.
            prompt_caching: entry
                .cost
                .as_ref()
                .map(|c| c.cache_read.is_some())
                .unwrap_or(false),
            reasoning_levels: entry.reasoning_levels(),
        },
    }
}

/// Best-effort enrichment of an already-resolved model with Models.dev data.
fn enrich(info: &mut ModelInfo, entry: &ModelEntry) {
    let md = entry_to_info(entry);
    info.merge_from(&md);
}

/// Resolve the available models for `provider` using the fallback chain.
/// Warnings explain any degradation; the source is always explicit.
pub async fn resolve_model_catalog(provider: &dyn Provider, mdc: &ModelsDevClient) -> ModelCatalog {
    let mut warnings: Vec<String> = Vec::new();
    let hint = provider.models_dev_hint();

    // ---- 1. Provider API -------------------------------------------------
    if provider.supports_model_listing() {
        match provider.list_models_api().await {
            Ok(Some(ids)) => {
                let mut models: Vec<ModelInfo> =
                    ids.into_iter().map(|id| ModelInfo::bare(&id)).collect();
                // Enrich with cached Models.dev metadata when possible.
                if let Ok(snapshot) = mdc.load().await {
                    for info in &mut models {
                        if let Some((_, entry)) =
                            snapshot.catalog.find_model_anywhere(&info.id, hint)
                        {
                            enrich(info, entry);
                        }
                    }
                } else {
                    warnings.push("Models.dev cache unavailable; metadata limited to ids".into());
                }
                // Apply local config overrides (custom providers).
                apply_local_overrides(provider, &mut models);
                return ModelCatalog {
                    models,
                    source: Some(ModelListSource::ProviderApi),
                    warnings,
                };
            }
            Ok(None) => warnings.push(format!(
                "provider {} does not expose a model listing endpoint",
                provider.id()
            )),
            Err(e) => warnings.push(format!("model listing failed: {e}; falling back")),
        }
    }

    // ---- 2. Models.dev ----------------------------------------------------
    let effective_hint = hint.or(Some(provider.id()));
    match mdc.load().await {
        Ok(snapshot) => {
            if snapshot.stale {
                warnings.push("Models.dev catalog is stale (fetch failed)".into());
            }
            if let Some(pid) = effective_hint {
                if let Some(prov_entry) = snapshot.catalog.get(pid) {
                    let mut models: Vec<ModelInfo> =
                        prov_entry.models.values().map(entry_to_info).collect();
                    models.sort_by(|a, b| a.id.cmp(&b.id));
                    apply_local_overrides(provider, &mut models);
                    return ModelCatalog {
                        models,
                        source: Some(ModelListSource::ModelsDev),
                        warnings,
                    };
                }
                warnings.push(format!("Models.dev has no entry for provider `{pid}`"));
            }

            // ---- 3. Local provider config ---------------------------------
            if !provider.local_models().is_empty() {
                let mut models: Vec<ModelInfo> =
                    provider.local_models().iter().map(local_to_info).collect();
                for info in &mut models {
                    if let Some((_, entry)) = snapshot.catalog.find_model_anywhere(&info.id, None) {
                        enrich(info, entry);
                    }
                }
                models.sort_by(|a, b| a.id.cmp(&b.id));
                return ModelCatalog {
                    models,
                    source: Some(ModelListSource::LocalConfig),
                    warnings,
                };
            }
        }
        Err(e) => warnings.push(format!(
            "Models.dev unavailable ({e}); falling back to local config"
        )),
    }

    // ---- 3b. Local config without Models.dev ------------------------------
    if !provider.local_models().is_empty() {
        let models: Vec<ModelInfo> = provider.local_models().iter().map(local_to_info).collect();
        return ModelCatalog {
            models,
            source: Some(ModelListSource::LocalConfig),
            warnings,
        };
    }

    ModelCatalog {
        models: Vec::new(),
        source: None,
        warnings,
    }
}

fn apply_local_overrides(provider: &dyn Provider, models: &mut [ModelInfo]) {
    if provider.local_models().is_empty() {
        return;
    }
    for info in models.iter_mut() {
        if let Some(lm) = provider.local_models().iter().find(|m| m.id == info.id) {
            let override_info = local_to_info(lm);
            info.merge_from(&override_info);
            // Explicit local family wins outright.
            if let Some(f) = &lm.family {
                info.family = Some(f.clone());
            }
        }
    }
}

/// Resolve pricing for one concrete provider+model route from Models.dev.
/// `Ok(None)` means: no price known — callers must record cost as null.
pub async fn load_pricing_context(
    provider_hint: Option<&str>,
    model_id: &str,
    mdc: &ModelsDevClient,
) -> Result<Option<PricingContext>, lmhub_core::CoreError> {
    let snapshot = mdc
        .load()
        .await
        .map_err(|e| lmhub_core::CoreError::Other(format!("models.dev: {e}")))?;
    Ok(pricing_context_in_snapshot(
        &snapshot,
        provider_hint,
        model_id,
    ))
}

/// Snapshot-loaded variant used by the TUI (loads once per selection).
pub fn pricing_context_in_snapshot(
    snapshot: &lmhub_modelsdev::CatalogSnapshot,
    provider_hint: Option<&str>,
    model_id: &str,
) -> Option<PricingContext> {
    let (_, entry) = snapshot
        .catalog
        .find_model_anywhere(model_id, provider_hint)?;
    let pricing = pricing_for(entry)?;
    Some(PricingContext {
        pricing,
        source: "models.dev".to_string(),
        fetched_at: Some(snapshot.fetched_at.clone()),
        snapshot_version: Some(snapshot.version.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_catalog_is_registered() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(AuthStore::load(dir.path().join("auth.json"))));
        let (registry, errors) = build_registry(dir.path(), store);
        assert!(errors.is_empty());
        // builtins(2) + catalog(~192 after dedupe of openai/anthropic)
        assert!(
            registry.all().len() >= 190,
            "expected >=190 providers, got {}",
            registry.all().len()
        );
        // spot checks across protocols
        for id in [
            "groq",
            "amazon-bedrock",
            "google",
            "github-copilot",
            "watsonx",
        ] {
            assert!(registry.get(id).is_some(), "missing {id}");
        }
        // custom TOML overrides known entry by id
        std::fs::write(
            dir.path().join("override.toml"),
            r#"
id = "groq"
name = "Groq Override"
api_type = "openai-compatible"
base_url = "https://my-proxy.example.com/v1"
api_key_env = "GROQ_API_KEY"
"#,
        )
        .unwrap();
        let (registry, _) = build_registry(dir.path(), {
            Arc::new(Mutex::new(AuthStore::load(dir.path().join("auth.json"))))
        });
        let groq = registry.get("groq").unwrap();
        assert_eq!(groq.display_name(), "Groq Override");
    }

    #[test]
    fn gitlab_duo_seeds_static_models() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(AuthStore::load(dir.path().join("auth.json"))));
        let (registry, _) = build_registry(dir.path(), store);
        let gitlab = registry.get("gitlab").unwrap();
        assert_eq!(gitlab.provider_type(), "gitlab-duo");
        assert!(!gitlab.local_models().is_empty());
    }
}
