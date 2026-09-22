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

/// Minimum secret length registered for scrubbing. Short values would match
/// too much ordinary text; the same rule is documented in the README.
pub const MIN_SECRET_LEN: usize = 8;

pub fn init() {
    // vars_os + lossy conversion: a non-UTF-8 env value must not panic the
    // runner (std::env::vars would).
    let env_values: Vec<String> = std::env::vars_os()
        .filter_map(|(k, v)| {
            let ku = k.to_string_lossy().to_ascii_uppercase();
            if !SENSITIVE_MARKERS.iter().any(|m| ku.contains(m)) {
                return None;
            }
            let value = v.to_string_lossy().into_owned();
            (value.len() >= MIN_SECRET_LEN).then_some(value)
        })
        .collect();
    // Hold the lock across merge+write: a concurrent `register_extra` must
    // not land between the read and the assignment (it would be clobbered).
    let mut secrets = lock();
    let mut values = env_values;
    values.extend(secrets.iter().cloned());
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    values.dedup();
    *secrets = values;
}

/// Register an additional secret value (auth.json entries, oauth tokens).
/// Safe to call before or after `init`; both paths are covered.
pub fn register_extra(value: &str) {
    if value.len() < MIN_SECRET_LEN {
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

    #[test]
    fn short_values_are_not_registered() {
        super::init();
        super::register_extra("1234567");
        assert_eq!(
            super::scrub("prefix-1234567-suffix"),
            "prefix-1234567-suffix"
        );
        super::register_extra("12345678");
        assert_eq!(
            super::scrub("prefix-12345678-suffix"),
            "prefix-[REDACTED]-suffix"
        );
    }
}
