//! Maintenance tasks for lmhub-runner.
//!
//! `cargo run -p xtask gen-providers` regenerates the bundled known-providers
//! snapshot from https://models.dev/api.json.

use sha2::{Digest, Sha256};

const CATALOG_URL: &str = "https://models.dev/api.json";
const OUT_PATH: &str = "crates/providers/src/known/known_providers.json";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let task = std::env::args().nth(1).unwrap_or_default();
    match task.as_str() {
        "gen-providers" => gen_providers().await,
        other => {
            eprintln!("usage: cargo run -p xtask -- gen-providers");
            if !other.is_empty() {
                anyhow::bail!("unknown task: {other}");
            }
            Ok(())
        }
    }
}

async fn gen_providers() -> anyhow::Result<()> {
    let raw = reqwest::get(CATALOG_URL)
        .await?
        .error_for_status()?
        .text()
        .await?;
    let catalog: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("invalid catalog JSON: {e}"))?;

    let mut providers: Vec<serde_json::Value> = Vec::new();
    let Some(entries) = catalog.as_object() else {
        anyhow::bail!("catalog root is not an object");
    };
    for (id, entry) in entries {
        providers.push(serde_json::json!({
            "id": id,
            "name": entry.get("name").and_then(|v| v.as_str()).unwrap_or(id),
            "npm": entry.get("npm").and_then(|v| v.as_str()),
            "api": entry.get("api").and_then(|v| v.as_str()),
            "env": entry.get("env").cloned().unwrap_or_else(|| serde_json::json!([])),
        }));
    }
    providers.sort_by_key(|p| p["id"].as_str().unwrap_or("").to_string());

    let payload_for_hash = serde_json::to_vec(&providers)?;
    let mut hasher = Sha256::new();
    hasher.update(&payload_for_hash);
    let version = hex::encode(hasher.finalize())[..16].to_string();

    let doc = serde_json::json!({
        "source": "models.dev",
        "snapshot_version": version,
        "providers": providers,
    });
    let rendered = serde_json::to_string_pretty(&doc)? + "\n";
    if let Some(parent) = std::path::Path::new(OUT_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(OUT_PATH, rendered)?;
    println!(
        "wrote {OUT_PATH}: {} providers, snapshot {version}",
        providers.len()
    );
    Ok(())
}
