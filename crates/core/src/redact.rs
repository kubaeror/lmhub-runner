use std::sync::{Mutex, OnceLock};

/// Values of runner environment variables whose names look like secrets.
/// Collected once at startup; used only to scrub them out of anything we
/// are about to persist. The values themselves never leave this module.
static SECRETS: OnceLock<Vec<String>> = OnceLock::new();
/// Extra values registered at runtime (e.g. keys from auth.json).
static EXTRA: Mutex<Vec<String>> = Mutex::new(Vec::new());

const SENSITIVE_MARKERS: [&str; 6] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"];

pub fn init() {
    let mut values: Vec<String> = std::env::vars()
        .filter(|(k, v)| {
            let ku = k.to_ascii_uppercase();
            SENSITIVE_MARKERS.iter().any(|m| ku.contains(m)) && v.len() >= 8
        })
        .map(|(_, v)| v)
        .collect();
    values.extend(EXTRA.lock().unwrap().iter().cloned());
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    let _ = SECRETS.set(values);
}

/// Register an additional secret value (auth.json entries, oauth tokens).
/// Safe to call before or after `init`; both paths are covered.
pub fn register_extra(value: &str) {
    if value.len() < 8 {
        return;
    }
    if let Some(secrets) = SECRETS.get() {
        // Rebuild the list with the new value first (longest-first ordering
        // is re-established on the combined set).
        let mut all: Vec<String> = secrets.clone();
        all.push(value.to_string());
        all.sort_by_key(|v| std::cmp::Reverse(v.len()));
        let _ = SECRETS.set(all);
    }
    EXTRA.lock().unwrap().push(value.to_string());
}

/// Replace any known secret value with `[REDACTED]`.
pub fn scrub(input: &str) -> String {
    let Some(secrets) = SECRETS.get() else {
        return input.to_string();
    };
    let mut out = input.to_string();
    for secret in secrets {
        if !secret.is_empty() && out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "[REDACTED]");
        }
    }
    out
}

/// True when the string looks like a bare API key we should never log.
pub fn looks_like_secret(input: &str) -> bool {
    let long_alnum = input.len() >= 20
        && input
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    long_alnum
        && (input.starts_with("sk-")
            || input.starts_with("Bearer ")
            || input.chars().filter(|c| c.is_ascii_digit()).count() > input.len() / 3)
}

#[cfg(test)]
mod tests {
    #[test]
    fn scrub_replaces_values() {
        std::env::set_var("LMHUB_TEST_SECRET_VALUE", "super-secret-value-123");
        super::init();
        let s = super::scrub("failed call with super-secret-value-123 inside");
        assert!(s.contains("[REDACTED]"));
        assert!(!s.contains("super-secret-value-123"));
    }
}
