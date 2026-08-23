//! Credential resolution: auth.json first, then environment variables.

use lmhub_core::{AuthStore, CoreError};

#[derive(Debug, Clone)]
pub struct ResolvedCredential {
    pub secret: String,
    /// Provenance label (`auth.json`, env var name).
    pub source: String,
}

/// Resolve a provider credential: stored auth.json entry wins over env vars.
pub fn resolve(
    store: &AuthStore,
    provider_id: &str,
    env_keys: &[String],
) -> Option<ResolvedCredential> {
    if let Some(secret) = store.secret_for(provider_id) {
        return Some(ResolvedCredential {
            secret,
            source: "auth.json".to_string(),
        });
    }
    for env_key in env_keys {
        if let Ok(value) = std::env::var(env_key) {
            if !value.trim().is_empty() && value.len() >= 8 {
                return Some(ResolvedCredential {
                    secret: value,
                    source: env_key.clone(),
                });
            }
        }
    }
    None
}

/// Standard error for a missing credential (never contains the secret).
pub fn missing_error(provider_id: &str, env_keys: &[String]) -> CoreError {
    let hint = env_keys
        .first()
        .cloned()
        .unwrap_or_else(|| "<config>".into());
    CoreError::MissingApiKey(format!(
        "{hint} (provider `{provider_id}` — set it via TUI `k` or env)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_json_wins_over_env() {
        std::env::set_var("LMHUB_TEST_PROVIDER_KEY", "env-value-1234567890");
        let dir = tempfile::tempdir().unwrap();
        let mut store = AuthStore::load(dir.path().join("auth.json"));
        store.set_credential(
            "test-provider",
            lmhub_core::StoredCredential::api("stored-value-12345"),
        );
        let got = resolve(&store, "test-provider", &["LMHUB_TEST_PROVIDER_KEY".into()]).unwrap();
        assert_eq!(got.secret, "stored-value-12345");
        assert_eq!(got.source, "auth.json");
    }

    #[test]
    fn env_fallback_when_no_store_entry() {
        std::env::set_var("LMHUB_TEST_OTHER_KEY", "env-value-1234567890");
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::load(dir.path().join("auth.json"));
        let got = resolve(
            &store,
            "other",
            &["MISSING_XYZ".into(), "LMHUB_TEST_OTHER_KEY".into()],
        )
        .unwrap();
        assert_eq!(got.source, "LMHUB_TEST_OTHER_KEY");
    }

    #[test]
    fn none_when_everything_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::load(dir.path().join("auth.json"));
        assert!(resolve(&store, "nope", &["DEFINITELY_NOT_SET_XYZ_123".into()]).is_none());
    }
}
