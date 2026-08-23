//! Provider adapters for lmhub-runner.
//!
//! - Native OpenAI and Anthropic adapters;
//! - config-driven OpenAI-compatible / Anthropic-compatible adapters
//!   loaded from `providers/*.toml` (custom providers need no core changes);
//! - the model-list resolution fallback chain (API → Models.dev → local).

pub mod anthropic;
pub mod azure;
pub mod bedrock;
pub mod cohere;
pub mod config;
pub mod copilot;
pub mod credentials;
pub mod gemini;
pub mod http;
pub mod known;
pub mod openai;
pub mod preauth;
pub mod registry;
pub mod routed;
pub mod sigv4;
pub mod sse;
pub mod stream_runner;
pub mod vertex;

mod wire_anthropic;
mod wire_openai;
#[cfg(test)]
mod wire_test_util;

pub use anthropic::NativeAnthropicProvider;
pub use config::{CustomProvider, CustomProviderConfig};
pub use http::{init_retry_policy, RetryPolicy};
pub use known::ProtocolKind;
pub use openai::NativeOpenAiProvider;
pub use registry::{
    build_registry, load_pricing_context, pricing_context_in_snapshot, resolve_model_catalog,
    ProviderRegistry,
};
pub use routed::RoutedProvider;
