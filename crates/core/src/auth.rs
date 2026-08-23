//! lmhub's own credential store: `~/.config/lmhub/auth.json`.
//!
//! Precedence everywhere: **auth.json > environment variables > missing**.
//! The file holds API keys (`type:"api"`) and OAuth token caches
//! (`type:"oauth"`, e.g. GitHub Copilot device flow). Values loaded here are
//! registered with `redact` so they can never leak into logs or statistics.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StoredCredential {
    /// `"api"` (static key) or `"oauth"` (token cache).
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

impl StoredCredential {
    pub fn api(key: impl Into<String>) -> Self {
        Self {
            kind: "api".into(),
            key: Some(key.into()),
            access_token: None,
            expires_at: None,
            refresh_token: None,
        }
    }

    /// The bearer value that should be used right now (oauth-aware).
    pub fn active_secret(&self) -> Option<&str> {
        if self.kind == "oauth" {
            let valid = self
                .expires_at
                .map(|exp| exp > chrono::Utc::now().timestamp())
                .unwrap_or(true);
            return if valid {
                self.access_token.as_deref()
            } else {
                self.refresh_token
                    .as_deref()
                    .or(self.access_token.as_deref())
            };
        }
        self.key.as_deref()
    }
}

#[derive(Debug, Clone)]
pub struct AuthStore {
    path: PathBuf,
    entries: BTreeMap<String, StoredCredential>,
}

impl AuthStore {
    pub fn path_for(config_dir: &Path) -> PathBuf {
        config_dir.join("auth.json")
    }

    /// Load the store; a missing file is an empty store. Broken JSON is also
    /// treated as empty (with a stderr note) so the runner always starts.
    pub fn load(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| {
                let parsed: Result<BTreeMap<String, StoredCredential>, _> =
                    serde_json::from_str(&raw);
                match parsed {
                    Ok(map) => Some(map),
                    Err(e) => {
                        eprintln!("lmhub: ignoring unreadable auth.json: {e}");
                        None
                    }
                }
            })
            .unwrap_or_default();
        Self { path, entries }
    }

    pub fn get(&self, provider_id: &str) -> Option<&StoredCredential> {
        self.entries.get(provider_id)
    }

    /// Best available secret for a provider ("api key or live oauth token").
    pub fn secret_for(&self, provider_id: &str) -> Option<String> {
        self.entries
            .get(provider_id)
            .and_then(|c| c.active_secret())
            .map(|s| s.to_string())
    }

    pub fn set_credential(&mut self, provider_id: impl Into<String>, cred: StoredCredential) {
        let id = provider_id.into();
        // Register with redact before anything can log it.
        if let Some(secret) = cred.key.as_deref() {
            crate::redact::register_extra(secret);
        }
        if let Some(secret) = cred.access_token.as_deref() {
            crate::redact::register_extra(secret);
        }
        self.entries.insert(id, cred);
    }

    /// Persist atomically with owner-only permissions on unix.
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let rendered = serde_json::to_string_pretty(&self.entries)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, rendered)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)
    }

    pub fn remove(&mut self, provider_id: &str) {
        self.entries.remove(provider_id);
    }

    /// Every secret value in the store (for redaction registration).
    pub fn all_secrets(&self) -> Vec<String> {
        self.entries
            .values()
            .filter_map(|c| {
                c.active_secret()
                    .map(|s| s.to_string())
                    .or_else(|| c.key.clone())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_precedence_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::load(path.clone());
        assert!(store.secret_for("x").is_none());
        store.set_credential("openai", StoredCredential::api("sk-test-abcdef123456"));
        store.save().unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let reloaded = AuthStore::load(path);
        assert_eq!(
            reloaded.secret_for("openai").as_deref(),
            Some("sk-test-abcdef123456")
        );
    }

    #[test]
    fn oauth_expiry_falls_back_to_refresh_token() {
        let expired = StoredCredential {
            kind: "oauth".into(),
            key: None,
            access_token: Some("old".into()),
            expires_at: Some(1), // far past
            refresh_token: Some("refresh".into()),
        };
        assert_eq!(expired.active_secret(), Some("refresh"));
    }
}
