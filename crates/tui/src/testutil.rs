//! Test helpers shared across the crate's unit tests (compiled only under
//! `#[cfg(test)]`). Builds a `State` backed by a tempdir so auth/config/
//! output writes never touch the real environment.

use crate::{State, TuiContext};

/// A state with the full provider registry, empty prefs and a tempdir for
/// auth/config/output. The tempdir is returned alongside so it outlives the
/// state for the whole test.
pub fn test_state() -> (State, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(std::sync::Mutex::new(lmhub_core::AuthStore::load(
        dir.path().join("auth.json"),
    )));
    let (registry, _) = lmhub_providers::build_registry(dir.path(), std::sync::Arc::clone(&store));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = TuiContext {
        registry,
        modelsdev: std::sync::Arc::new(lmhub_modelsdev::ModelsDevClient::new(
            dir.path().join("cache"),
            std::time::Duration::from_secs(60),
        )),
        config: lmhub_core::AppConfig::default(),
        config_path: dir.path().join("config.toml"),
        prompts: Vec::new(),
        task_prompts: Vec::new(),
        output_base: dir.path().join("output"),
        auth_store: store,
        sandbox_runtime: lmhub_sandbox::SandboxRuntime::Legacy,
    };
    (State::new(ctx, tx), dir)
}
