//! Reasoning map: every model across all providers with its supported
//! reasoning levels — built from the full Models.dev snapshot (plus any
//! locally cached catalogs for custom providers). Pure logic, no ratatui.

use crate::state::CachedCatalog;
use lmhub_core::ReasoningLevel;
use lmhub_modelsdev::{catalog::ModelEntry, CatalogSnapshot};
use std::collections::BTreeMap;

/// One model row in the reasoning map.
#[derive(Debug, Clone)]
pub struct MapModel {
    pub provider_id: String,
    pub model_id: String,
    /// `None` = no declaration → every level is offered.
    pub levels: Option<Vec<ReasoningLevel>>,
    /// Whether the model supports reasoning at all.
    pub reasoning: bool,
}

impl MapModel {
    fn from_entry(provider_id: &str, m: &ModelEntry) -> Self {
        Self {
            provider_id: provider_id.to_string(),
            model_id: m.id.clone(),
            levels: m.reasoning_levels(),
            reasoning: m.reasoning.unwrap_or(false),
        }
    }
}

/// All models from the snapshot, then locally cached models not covered by
/// it (custom providers). Sorted by provider id, then model id.
pub fn all_models(
    snapshot: &CatalogSnapshot,
    extra: &BTreeMap<String, CachedCatalog>,
) -> Vec<MapModel> {
    let mut rows: Vec<MapModel> = Vec::new();
    let mut seen: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    for (pid, entry) in &snapshot.catalog.providers {
        for (mid, mentry) in &entry.models {
            seen.insert((pid.clone(), mid.clone()));
            rows.push(MapModel::from_entry(pid, mentry));
        }
    }
    for (pid, cache) in extra {
        for model in &cache.models {
            if seen.insert((pid.clone(), model.id.clone())) {
                rows.push(MapModel {
                    provider_id: pid.clone(),
                    model_id: model.id.clone(),
                    levels: model.capabilities.reasoning_levels.clone(),
                    reasoning: model.capabilities.reasoning,
                });
            }
        }
    }
    rows.sort_by(|a, b| {
        a.provider_id
            .cmp(&b.provider_id)
            .then(a.model_id.cmp(&b.model_id))
    });
    rows
}

/// Keep models whose id or provider matches the query (case-insensitive
/// substring on either).
pub fn filtered(models: &[MapModel], query: &str) -> Vec<MapModel> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return models.to_vec();
    }
    models
        .iter()
        .filter(|m| {
            m.model_id.to_ascii_lowercase().contains(&q)
                || m.provider_id.to_ascii_lowercase().contains(&q)
        })
        .cloned()
        .collect()
}

/// The levels actually offered for a model — the same semantics as the
/// setup pane: no reasoning → off only; empty declaration → off only;
/// no declaration → all levels.
/// Levels offered for one model: `off`-only when it does not reason, the
/// declared set when present, every level otherwise. Single source of truth
/// for both the setup pane (`level`s_for`) and the reasoning map.
pub fn offered_levels(reasoning: bool, declared: Option<&[ReasoningLevel]>) -> Vec<ReasoningLevel> {
    if !reasoning {
        return vec![ReasoningLevel::Off];
    }
    match declared {
        Some(levels) if levels.is_empty() => vec![ReasoningLevel::Off],
        Some(levels) => levels.to_vec(),
        None => ReasoningLevel::ALL.to_vec(),
    }
}

/// Levels offered for a map row.
pub fn effective_levels(m: &MapModel) -> Vec<ReasoningLevel> {
    offered_levels(m.reasoning, m.levels.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmhub_modelsdev::catalog::{Catalog, ProviderEntry};

    #[test]
    fn offered_levels_cover_all_capability_shapes() {
        let off = vec![ReasoningLevel::Off];
        assert_eq!(offered_levels(false, None), off);
        assert_eq!(offered_levels(false, Some(&[])), off);
        assert_eq!(offered_levels(true, Some(&[])), off);
        assert_eq!(
            offered_levels(true, Some(&[ReasoningLevel::Off, ReasoningLevel::High])),
            vec![ReasoningLevel::Off, ReasoningLevel::High]
        );
        assert_eq!(offered_levels(true, None), ReasoningLevel::ALL.to_vec());
    }

    fn model_entry(id: &str, options: serde_json::Value) -> ModelEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "reasoning": true,
            "reasoning_options": options,
        }))
        .unwrap()
    }

    #[test]
    fn snapshot_builds_full_map() {
        let catalog = Catalog {
            providers: BTreeMap::from([
                (
                    "openai".into(),
                    ProviderEntry {
                        id: "openai".into(),
                        name: "OpenAI".into(),
                        env: vec![],
                        api: None,
                        doc: None,
                        npm: None,
                        models: BTreeMap::from([
                            (
                                "gpt-4o".into(),
                                model_entry("gpt-4o", serde_json::json!([])),
                            ),
                            (
                                "gpt-4.1".into(),
                                model_entry(
                                    "gpt-4.1",
                                    serde_json::json!([{ "type": "effort", "values": ["low", "high"] }]),
                                ),
                            ),
                        ]),
                    },
                ),
                (
                    "anthropic".into(),
                    ProviderEntry {
                        id: "anthropic".into(),
                        name: "Anthropic".into(),
                        env: vec![],
                        api: None,
                        doc: None,
                        npm: None,
                        models: BTreeMap::from([(
                            "claude-3-7-sonnet".into(),
                            model_entry(
                                "claude-3-7-sonnet",
                                serde_json::json!([{ "type": "budget_tokens", "min": 1024 }]),
                            ),
                        )]),
                    },
                ),
            ]),
        };
        let snapshot = CatalogSnapshot {
            catalog,
            fetched_at: "t".into(),
            version: "v".into(),
            stale: false,
        };
        let rows = all_models(&snapshot, &BTreeMap::new());
        assert_eq!(rows.len(), 3);
        // Reasoning declarations parsed from reasoning_options.
        let gpt41 = rows.iter().find(|m| m.model_id == "gpt-4.1").unwrap();
        assert_eq!(
            gpt41.levels,
            Some(vec![ReasoningLevel::Low, ReasoningLevel::High])
        );
        assert!(gpt41.reasoning);
        let claude = rows
            .iter()
            .find(|m| m.model_id == "claude-3-7-sonnet")
            .unwrap();
        assert!(claude.reasoning, "budget_tokens counts as reasoning");
    }

    #[test]
    fn empty_declaration_means_off_only() {
        let m = MapModel {
            provider_id: "p".into(),
            model_id: "m".into(),
            levels: Some(vec![]),
            reasoning: true,
        };
        assert_eq!(effective_levels(&m), vec![ReasoningLevel::Off]);
        let m2 = MapModel {
            provider_id: "p".into(),
            model_id: "m".into(),
            levels: None,
            reasoning: true,
        };
        assert_eq!(effective_levels(&m2).len(), ReasoningLevel::ALL.len());
        let m3 = MapModel {
            provider_id: "p".into(),
            model_id: "m".into(),
            levels: Some(vec![ReasoningLevel::Off, ReasoningLevel::High]),
            reasoning: true,
        };
        assert_eq!(
            effective_levels(&m3),
            vec![ReasoningLevel::Off, ReasoningLevel::High]
        );
    }

    #[test]
    fn filter_matches_model_or_provider() {
        let rows = vec![
            MapModel {
                provider_id: "openai".into(),
                model_id: "gpt-4o".into(),
                levels: None,
                reasoning: true,
            },
            MapModel {
                provider_id: "groq".into(),
                model_id: "llama-3.3".into(),
                levels: None,
                reasoning: true,
            },
        ];
        assert_eq!(filtered(&rows, "GPT").len(), 1);
        assert_eq!(filtered(&rows, "groq").len(), 1);
        assert_eq!(filtered(&rows, "").len(), 2);
        assert_eq!(filtered(&rows, "zzz").len(), 0);
    }
}
