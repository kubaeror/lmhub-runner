//! Models.dev catalog client for lmhub-runner.
//!
//! Fetches <https://models.dev/api.json>, caches it locally (TTL) and serves
//! stale data when the network is unavailable. Pricing is always tied to a
//! concrete provider+model route — never to a family alone.

pub mod catalog;

use catalog::Catalog;
use lmhub_core::pricing::ModelPricing;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CATALOG_URL: &str = "https://models.dev/api.json";

#[derive(Debug, thiserror::Error)]
pub enum ModelsDevError {
    #[error("fetch failed: {0}")]
    Fetch(String),
    #[error("cache io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cache parse error: {0}")]
    Parse(String),
}

/// Provenance metadata of a loaded catalog snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub fetched_at: String,
    pub version: String,
}

/// A catalog plus its provenance. `stale == true` means the network refresh
/// failed and an older cached copy was served — callers should warn.
pub struct CatalogSnapshot {
    pub catalog: Catalog,
    pub fetched_at: String,
    /// Short sha256 of the raw JSON; used as the pricing snapshot version.
    pub version: String,
    pub stale: bool,
}

/// Cached Models.dev client.
///
/// The runner must not fetch the catalog on every run: responses live in
/// `~/.cache/lmhub/` and are refreshed only when older than the TTL, with a
/// stale-cache fallback when the network fails.
pub struct ModelsDevClient {
    http: reqwest::Client,
    cache_dir: PathBuf,
    ttl: Duration,
}

impl ModelsDevClient {
    pub fn new(cache_dir: PathBuf, ttl: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("lmhub-runner/0.1 (+https://models.dev)")
            .build()
            .expect("reqwest client builds");
        Self {
            http,
            cache_dir,
            ttl,
        }
    }

    fn data_path(&self) -> PathBuf {
        self.cache_dir.join("models.dev.json")
    }

    fn meta_path(&self) -> PathBuf {
        self.cache_dir.join("models.dev.meta.json")
    }

    async fn read_cache(&self) -> Option<(String, SnapshotMeta)> {
        let data = tokio::fs::read_to_string(self.data_path()).await.ok()?;
        let meta_raw = tokio::fs::read_to_string(self.meta_path()).await.ok()?;
        let meta: SnapshotMeta = serde_json::from_str(&meta_raw).ok()?;
        Some((data, meta))
    }

    async fn write_cache(&self, raw: &str) -> Result<SnapshotMeta, ModelsDevError> {
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        let version = hex::encode(hasher.finalize())[..16].to_string();
        let meta = SnapshotMeta {
            fetched_at: lmhub_core::now_ts(),
            version,
        };
        let tmp_data = self.data_path().with_extension("tmp");
        let tmp_meta = self.meta_path().with_extension("tmp");
        tokio::fs::write(&tmp_data, raw).await?;
        tokio::fs::write(
            &tmp_meta,
            serde_json::to_string_pretty(&meta)
                .map_err(|e| ModelsDevError::Parse(e.to_string()))?,
        )
        .await?;
        tokio::fs::rename(&tmp_data, self.data_path()).await?;
        tokio::fs::rename(&tmp_meta, self.meta_path()).await?;
        Ok(meta)
    }

    async fn fetch_fresh(&self) -> Result<(String, SnapshotMeta), ModelsDevError> {
        let resp = self
            .http
            .get(CATALOG_URL)
            .send()
            .await
            .map_err(|e| ModelsDevError::Fetch(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ModelsDevError::Fetch(format!(
                "HTTP {} from {CATALOG_URL}",
                resp.status()
            )));
        }
        let raw = resp
            .text()
            .await
            .map_err(|e| ModelsDevError::Fetch(e.to_string()))?;
        // Validate before caching so we never persist garbage over a good copy.
        Catalog::parse(&raw).map_err(|e| ModelsDevError::Parse(e.to_string()))?;
        let meta = self.write_cache(&raw).await?;
        Ok((raw, meta))
    }

    /// Load the catalog honoring the TTL.
    pub async fn load(&self) -> Result<CatalogSnapshot, ModelsDevError> {
        if let Some((data, meta)) = self.read_cache().await {
            if cache_age_ok(&meta, self.ttl) {
                return Ok(build_snapshot(data, meta, false));
            }
        }
        match self.fetch_fresh().await {
            Ok((data, meta)) => Ok(build_snapshot(data, meta, false)),
            Err(fetch_err) => {
                if let Some((data, meta)) = self.read_cache().await {
                    tracing::warn!(
                        error = %fetch_err,
                        "Models.dev fetch failed; serving stale cached catalog"
                    );
                    Ok(build_snapshot(data, meta, true))
                } else {
                    Err(fetch_err)
                }
            }
        }
    }

    /// Force a network refresh (TUI action).
    pub async fn refresh(&self) -> Result<CatalogSnapshot, ModelsDevError> {
        let (data, meta) = self.fetch_fresh().await?;
        Ok(build_snapshot(data, meta, false))
    }

    pub fn cache_paths(&self) -> (PathBuf, PathBuf) {
        (self.data_path(), self.meta_path())
    }
}

fn build_snapshot(raw: String, meta: SnapshotMeta, stale: bool) -> CatalogSnapshot {
    let catalog = Catalog::parse(&raw).unwrap_or_default();
    CatalogSnapshot {
        catalog,
        fetched_at: meta.fetched_at,
        version: meta.version,
        stale,
    }
}

fn cache_age_ok(meta: &SnapshotMeta, ttl: Duration) -> bool {
    match chrono::DateTime::parse_from_rfc3339(&meta.fetched_at) {
        Ok(ts) => {
            let age = chrono::Utc::now().signed_duration_since(ts.with_timezone(&chrono::Utc));
            age.num_seconds() >= 0 && age.to_std().unwrap_or_default() < ttl
        }
        Err(_) => false,
    }
}

/// Price for one concrete provider+model route, or `None` when Models.dev
/// has no usable price (callers must record cost as null + warning).
pub fn pricing_for(entry: &catalog::ModelEntry) -> Option<ModelPricing> {
    let cost = entry.cost.as_ref()?;
    ModelPricing::from_parts(cost.input, cost.output, cost.cache_read, cost.cache_write)
}

/// Ensure the cache directory exists.
pub fn ensure_cache_dir(cache_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(cache_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CostBlock, ModelEntry};

    const SAMPLE: &str = r#"{
      "anthropic": {
        "id": "anthropic", "name": "Anthropic", "env": ["ANTHROPIC_API_KEY"],
        "models": {
          "claude-sonnet-4-5": {
            "id": "claude-sonnet-4-5", "name": "Claude Sonnet 4.5",
            "family": "claude-sonnet", "reasoning": true, "tool_call": true,
            "limit": {"context": 1000000, "output": 64000},
            "cost": {"input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75}
          }
        }
      },
      "unknown-provider-x": {"id":"x","name":"X","env":[],"models":{}}
    }"#;

    #[test]
    fn parses_sample_catalog() {
        let cat = Catalog::parse(SAMPLE).unwrap();
        assert_eq!(cat.providers.len(), 2);
        let m = cat.model("anthropic", "claude-sonnet-4-5").unwrap();
        assert_eq!(m.family.as_deref(), Some("claude-sonnet"));
        assert_eq!(m.limit.as_ref().unwrap().context, Some(1_000_000));
    }

    #[test]
    fn pricing_requires_base_prices() {
        let cat = Catalog::parse(SAMPLE).unwrap();
        let m = cat.model("anthropic", "claude-sonnet-4-5").unwrap();
        let p = pricing_for(m).unwrap();
        assert_eq!(p.input_per_million_usd, 3.0);
        assert_eq!(p.cache_read_per_million_usd, Some(0.3));

        let no_price = ModelEntry {
            id: "m".into(),
            name: "M".into(),
            cost: Some(CostBlock::default()),
            ..Default::default()
        };
        assert!(pricing_for(&no_price).is_none(), "no guessing");
    }

    #[test]
    fn finds_models_anywhere_with_preference() {
        let cat = Catalog::parse(SAMPLE).unwrap();
        let (pid, _) = cat.find_model_anywhere("claude-sonnet-4-5", None).unwrap();
        assert_eq!(pid, "anthropic");
        assert!(cat.find_model_anywhere("does-not-exist", None).is_none());
    }

    #[test]
    fn ttl_logic() {
        let fresh = SnapshotMeta {
            fetched_at: lmhub_core::now_ts(),
            version: "abc".into(),
        };
        assert!(cache_age_ok(&fresh, Duration::from_secs(3600)));
        let old = SnapshotMeta {
            fetched_at: "2020-01-01T00:00:00Z".into(),
            version: "abc".into(),
        };
        assert!(!cache_age_ok(&old, Duration::from_secs(3600)));
    }

    /// Live connectivity check against the real catalog.
    /// Run explicitly: `cargo test -p lmhub-modelsdev -- --ignored`
    #[tokio::test]
    #[ignore = "requires network access"]
    async fn live_fetch_parses_real_catalog() {
        let dir =
            std::env::temp_dir().join(format!("lmhub-mdtest-{}", uuid::Uuid::new_v4().simple()));
        let client = ModelsDevClient::new(dir.clone(), Duration::from_secs(3600));
        let snapshot = client.load().await.expect("live fetch should work");
        assert!(!snapshot.catalog.providers.is_empty());
        assert!(!snapshot.version.is_empty());

        // Concrete provider+model route must resolve with real prices.
        let (_, entry) = snapshot
            .catalog
            .find_model_anywhere("claude-sonnet-4-5", Some("anthropic"))
            .expect("claude-sonnet-4-5 present in catalog");
        let pricing = pricing_for(entry).expect("price known");
        assert!(pricing.input_per_million_usd > 0.0);

        let _ = std::fs::remove_dir_all(dir);
    }
}
