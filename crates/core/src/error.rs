#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("provider API error: {0}")]
    Provider(String),
    /// Retryable upstream condition (429/5xx/transport). Classified by the
    /// HTTP layer; carries structured hints instead of prose-only errors.
    #[error("transient provider error: {message}")]
    Transient {
        code: Option<u16>,
        retry_after_secs: Option<u64>,
        message: String,
    },
    #[error("http transport error: {0}")]
    Http(String),
    #[error("missing API key environment variable `{0}`")]
    MissingApiKey(String),
    #[error("response parsing failed: {0}")]
    Parse(String),
    #[error("sandbox violation: {0}")]
    Sandbox(String),
    #[error("operation timed out")]
    Timeout,
    #[error("operation cancelled")]
    Cancelled,
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// Stable machine-readable kind used in events and errors.log.
impl CoreError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Provider(_) => "provider_api",
            Self::Transient { .. } => "provider_api",
            Self::Http(_) => "http_transport",
            Self::MissingApiKey(_) => "missing_api_key",
            Self::Parse(_) => "response_parsing",
            Self::Sandbox(_) => "sandbox_violation",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::LimitExceeded(_) => "limit_exceeded",
            Self::Io(_) => "io",
            Self::Other(_) => "other",
        }
    }
}

/// UTC timestamp formatted for logs/events.
pub fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
