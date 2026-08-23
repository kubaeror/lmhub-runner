use std::sync::{Mutex, MutexGuard};

/// Values of runner environment variables whose names look like secrets.
/// Collected once at startup; used only to scrub them out of anything we
/// are about to persist. The values themselves never leave this module.
/// A plain `Mutex` (not `OnceLock`) so values registered at runtime
/// (`register_extra`) are visible to `scrub` no matter when they arrive.
static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

const SENSITIVE_MARKERS: [&str; 6] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"];

fn lock() -> MutexGuard<'static, Vec<String>> {
    SECRETS.lock().expect("redact secrets mutex poisoned")
}

pub fn init() {
    let mut values: Vec<String> = std::env::vars()
        .filter(|(k, v)| {
            let ku = k.to_ascii_uppercase();
            SENSITIVE_MARKERS.iter().any(|m| ku.contains(m)) && v.len() >= 8
        })
        .map(|(_, v)| v)
        .collect();
    values.extend(lock().iter().cloned());
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    values.dedup();
    *lock() = values;
}

/// Register an additional secret value (auth.json entries, oauth tokens).
/// Safe to call before or after `init`; both paths are covered.
pub fn register_extra(value: &str) {
    if value.len() < 8 {
        return;
    }
    let mut secrets = lock();
    if !secrets.iter().any(|s| s == value) {
        secrets.push(value.to_string());
        secrets.sort_by_key(|v| std::cmp::Reverse(v.len()));
    }
}

/// Replace any known secret value with `[REDACTED]`.
pub fn scrub(input: &str) -> String {
    let secrets = lock();
    let mut out = input.to_string();
    for secret in secrets.iter() {
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

    #[test]
    fn scrub_replaces_values_registered_after_init() {
        super::init();
        super::register_extra("runtime-secret-value-456");
        let s = super::scrub("logged key runtime-secret-value-456 leaked");
        assert!(s.contains("[REDACTED]"));
        assert!(!s.contains("runtime-secret-value-456"));
    }

    #[test]
    fn register_extra_before_init_is_kept() {
        super::register_extra("early-secret-value-789");
        super::init();
        let s = super::scrub("early-secret-value-789 in an error");
        assert!(s.contains("[REDACTED]"));
    }
}