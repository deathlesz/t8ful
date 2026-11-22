/// Failures of a provider request.
///
/// Messages are user-facing: the TUI and headless renderers print them
/// unchanged, so they must stay actionable and free of response bodies that
/// could contain sensitive data.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error(
        "missing provider credentials: set OPENROUTER_API_KEY or OPENCODE_GO_API_KEY, or run /login"
    )]
    MissingCredentials,

    #[error("provider authentication failed: {0}")]
    Auth(String),

    #[error("model is not available: {0}")]
    UnsupportedModel(String),

    #[error("provider rate limited the request: {message}")]
    RateLimited {
        message: String,
        retry_after_ms: Option<u64>,
    },

    /// The provider refused to generate because the account cannot be billed.
    ///
    /// `limit_source` is the provider's own name for the limit that was hit, so
    /// the retry decision can tell a limit that frees up on its own from one
    /// that can only be fixed by the user adding credits.
    #[error("provider payment required: {message}")]
    PaymentRequired {
        message: String,
        retry_after_ms: Option<u64>,
        limit_source: Option<String>,
    },

    #[error("provider returned HTTP {status}: {message}")]
    Upstream { status: u16, message: String },

    #[error("transport error: {0}")]
    Transport(String),

    #[error("malformed provider stream: {0}")]
    MalformedStream(String),

    #[error("model produced invalid arguments for tool call {index}: {message}")]
    MalformedToolCall { index: u32, message: String },

    #[error("provider stream ended before completion: {0}")]
    IncompleteStream(String),

    #[error("provider request cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

/// Failures of one agent turn.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Provider(#[from] ProviderError),

    #[error("turn cancelled")]
    Cancelled,

    #[error("event consumer disconnected during the turn")]
    EventSinkClosed,

    #[error("session journal failed: {0}")]
    Journal(String),

    #[error("context compaction failed: {0}")]
    Compaction(String),

    #[error("input required: {0}")]
    InputRequired(String),
}
