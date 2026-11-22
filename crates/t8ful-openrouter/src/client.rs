use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use t8ful_core::error::ProviderError;
use t8ful_core::provider::{ModelInfo, Provider, ProviderRequest, ProviderStream};
use t8ful_openai::client::{HttpStream, Secret, build_http_client, read_capped_body};
use t8ful_openai::error::check_response;
use t8ful_openai::profile::ProtocolProfile;
use t8ful_openai::pump::{MAX_MODEL_CATALOG_BYTES, STREAM_IDLE_TIMEOUT, pump_stream};
use tokio::sync::mpsc;

use crate::models::ModelList;

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Sent on every request, so OpenRouter can attribute traffic and its rankings
/// credit t8ful by name. The referer and title are public in that ranking.
pub const USER_AGENT: &str = concat!("t8ful/", env!("CARGO_PKG_VERSION"));
pub const REFERER: &str = "https://github.com/deathlesz/t8ful";
pub const TITLE: &str = "t8ful";
pub const CATEGORIES: &str = "cli-agent";

const STREAM_CHANNEL_CAPACITY: usize = 64;

/// Credential wrapper that never leaks its value through `Debug`.
pub use t8ful_openai::client::Secret as Redacted;

/// OpenRouter HTTP client implementing the core provider contract.
pub struct OpenRouterClient {
    http: reqwest::Client,
    base_url: String,
    /// `None` when the client was built without a credential: every request
    /// then fails with [`ProviderError::MissingCredentials`] instead of sending
    /// an unauthenticated call.
    api_key: Option<Secret>,
    /// Idle deadline for a chat stream, see [`STREAM_IDLE_TIMEOUT`].
    stream_idle_timeout: Duration,
}

impl fmt::Debug for OpenRouterClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenRouterClient")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .finish()
    }
}

impl OpenRouterClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self, ProviderError> {
        Self::with_base_url(DEFAULT_BASE_URL, api_key)
    }

    /// Build a client against a specific base URL, which tests point at a local
    /// fake server.
    pub fn with_base_url(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            http: build_http_client()?,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: Some(Secret::new(api_key)),
            stream_idle_timeout: STREAM_IDLE_TIMEOUT,
        })
    }

    /// Build a client without credentials.
    ///
    /// The terminal interface starts this way so it can show "not connected"
    /// and accept `/login`; the first request fails with
    /// [`ProviderError::MissingCredentials`].
    pub fn unauthenticated(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        Ok(Self {
            http: build_http_client()?,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: None,
            stream_idle_timeout: STREAM_IDLE_TIMEOUT,
        })
    }

    /// Read credentials from the environment. There is no fallback and no
    /// default model.
    pub fn from_env() -> Result<Self, ProviderError> {
        Self::from_api_key(std::env::var("OPENROUTER_API_KEY").ok(), DEFAULT_BASE_URL)
    }

    /// Resolve an optional credential, keeping "missing" distinct from "empty".
    pub fn from_api_key(
        api_key: Option<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        match api_key {
            Some(key) if !key.trim().is_empty() => Self::with_base_url(base_url, key),
            _ => Err(ProviderError::MissingCredentials),
        }
    }

    /// Resolve an optional credential into a usable client, keeping the client
    /// unauthenticated rather than failing to build.
    pub fn from_optional_api_key(
        api_key: Option<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        match api_key {
            Some(key) if !key.trim().is_empty() => Self::with_base_url(base_url, key),
            _ => Self::unauthenticated(base_url),
        }
    }

    pub fn with_base_url_from_env(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let api_key =
            std::env::var("OPENROUTER_API_KEY").map_err(|_| ProviderError::MissingCredentials)?;

        Self::with_base_url(base_url, api_key)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether a credential is configured.
    pub fn has_credentials(&self) -> bool {
        self.api_key.is_some()
    }

    /// Replace the idle deadline, so a test can observe it without waiting.
    pub fn with_stream_idle_timeout(mut self, timeout: Duration) -> Self {
        self.stream_idle_timeout = timeout;
        self
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url)
    }

    /// Attribution headers, applied to every request including the catalog.
    ///
    /// Nothing here is secret; `X-OpenRouter-Categories` is what puts t8ful in
    /// OpenRouter's `cli-agent` ranking.
    fn attribution(&self) -> [(&'static str, String); 4] {
        attribution_headers()
    }

    fn authorized(&self) -> Result<&Secret, ProviderError> {
        self.api_key
            .as_ref()
            .ok_or(ProviderError::MissingCredentials)
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let api_key = self.authorized()?;
        let mut request = self
            .http
            .get(self.endpoint("models"))
            .bearer_auth(api_key.expose());

        for (name, value) in self.attribution() {
            request = request.header(name, value);
        }

        let response = request
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let response = check_response(response).await?;
        let body = read_capped_body(response, MAX_MODEL_CATALOG_BYTES).await?;
        let list: ModelList = serde_json::from_slice(&body).map_err(|error| {
            ProviderError::MalformedStream(format!("invalid model list: {error}"))
        })?;

        Ok(list.into_models())
    }

    fn open_stream(&self, request: ProviderRequest) -> ProviderStream {
        let http = self.http.clone();
        let url = self.endpoint(ProtocolProfile::OPENROUTER.chat_path);
        let api_key = self.api_key.clone();
        let idle = self.stream_idle_timeout;
        let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);

        let task = tokio::spawn(async move {
            let result = match api_key {
                Some(api_key) => {
                    pump_stream(
                        &http,
                        &url,
                        &attribution_headers(),
                        &api_key,
                        &request,
                        &ProtocolProfile::OPENROUTER,
                        idle,
                        &sender,
                    )
                    .await
                }
                None => Err(ProviderError::MissingCredentials),
            };

            if let Err(error) = result {
                let _ = sender.send(Err(error)).await;
            }
        });

        Box::pin(HttpStream::new(receiver, task))
    }
}

/// Every attribution header, in one place so the catalog and the chat request
/// cannot drift.
fn attribution_headers() -> [(&'static str, String); 4] {
    [
        ("User-Agent", USER_AGENT.to_owned()),
        ("HTTP-Referer", REFERER.to_owned()),
        ("X-OpenRouter-Title", TITLE.to_owned()),
        ("X-OpenRouter-Categories", CATEGORIES.to_owned()),
    ]
}

#[async_trait]
impl Provider for OpenRouterClient {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.models().await
    }

    fn stream(&self, request: ProviderRequest) -> ProviderStream {
        self.open_stream(request)
    }
}
