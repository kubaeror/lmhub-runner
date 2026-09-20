//! Maintenance tasks for lmhub-runner.
//!
//! `cargo run -p xtask gen-providers` regenerates the bundled known-providers
//! snapshot from https://models.dev/api.json.
//!
//! `cargo run -p xtask sync-prompts` regenerates `prompts/default.md` and
//! `prompts/task-prompts/default.md` from the canonical constants in
//! `lmhub-core` (CI fails on drift).

use sha2::{Digest, Sha256};

const CATALOG_URL: &str = "https://models.dev/api.json";
const OUT_PATH: &str = "crates/providers/src/known/known_providers.json";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let task = std::env::args().nth(1).unwrap_or_default();
    match task.as_str() {
        "gen-providers" => gen_providers().await,
        "sync-prompts" => sync_prompts(),
        other => {
            eprintln!("usage: cargo run -p xtask -- gen-providers | sync-prompts");
            if !other.is_empty() {
                anyhow::bail!("unknown task: {other}");
            }
            Ok(())
        }
    }
}

/// The canonical prompt text lives in `lmhub-core`; these files are generated
/// copies so the release tarball and first-run materialization ship the same
/// text the code embeds.
fn sync_prompts() -> anyhow::Result<()> {
    let files: [(&str, &str); 2] = [
        ("prompts/default.md", lmhub_core::DEFAULT_SYSTEM_PROMPT),
        (
            "prompts/task-prompts/default.md",
            lmhub_core::DEFAULT_TASK_PROMPT,
        ),
    ];
    for (path, contents) in files {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("{}\n", contents.trim_end()))?;
        println!("wrote {path}");
    }
    Ok(())
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
        let env: Vec<String> = entry
            .get("env")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        // Only credential-bearing names: using the raw `env` order made
        // `AZURE_RESOURCE_NAME` an "API key" (see known::classify_key_envs).
        let key_envs = lmhub_providers::known::classify_key_envs(id, &env);
        providers.push(serde_json::json!({
            "id": id,
            "name": entry.get("name").and_then(|v| v.as_str()).unwrap_or(id),
            "npm": entry.get("npm").and_then(|v| v.as_str()),
            "api": entry.get("api").and_then(|v| v.as_str()),
            "env": env,
            "key_envs": key_envs,
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
