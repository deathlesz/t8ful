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

/// The provider id t8ful uses for this catalog.
pub const PROVIDER_ID: &str = "opencode-go";

/// Go serves catalog and chat under one base.
pub const DEFAULT_BASE_URL: &str = "https://opencode.ai/inference/go";

/// Attribution header. Go has no ranking program; the user agent only names the
/// client so a server-side problem can be traced back to a t8ful version.
pub const USER_AGENT: &str = concat!("t8ful/", env!("CARGO_PKG_VERSION"));

/// Header name Go requires on every chat request.
pub const SESSION_HEADER: &str = "x-opencode-session";

const STREAM_CHANNEL_CAPACITY: usize = 64;

/// The `x-opencode-session` value for one conversation.
///
/// Go rejects a chat request without it with a 400 and also uses it to route
/// prompt caching, so it must stay stable for the life of a conversation. The
/// agent's session id is the natural source; a run without one gets a value
/// derived from the process, which is stable for as long as that process lives.
pub fn session_header(session_id: Option<&str>) -> String {
    match session_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => {
            let id = id.strip_prefix("ses_").unwrap_or(id);

            format!("ses_{id}")
        }
        None => format!("ses_process-{}", std::process::id()),
    }
}

/// OpenCode Go HTTP client implementing the core provider contract.
pub struct OpenCodeGoClient {
    http: reqwest::Client,
    base_url: String,
    /// `None` when the client was built without a credential: every request
    /// then fails with [`ProviderError::MissingCredentials`] instead of sending
    /// an unauthenticated call.
    api_key: Option<Secret>,
    stream_idle_timeout: Duration,
}

impl fmt::Debug for OpenCodeGoClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenCodeGoClient")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .finish()
    }
}

impl OpenCodeGoClient {
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
    /// and accept `/login opencode-go`; the first request fails with
    /// [`ProviderError::MissingCredentials`].
    pub fn unauthenticated(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        Ok(Self {
            http: build_http_client()?,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: None,
            stream_idle_timeout: STREAM_IDLE_TIMEOUT,
        })
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

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

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

    async fn models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        // The catalog tolerates an anonymous request, so a key is used when one
        // is configured and left out otherwise.
        let mut request = self.http.get(self.endpoint("v1/models"));

        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key.expose());
        }

        let response = request
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    ProviderError::Transport("the provider request timed out".to_owned())
                } else {
                    ProviderError::Transport(error.to_string())
                }
            })?;
        let response = check_response(response).await?;
        let body = read_capped_body(response, MAX_MODEL_CATALOG_BYTES).await?;
        let list: ModelList = serde_json::from_slice(&body).map_err(|error| {
            ProviderError::MalformedStream(format!("invalid model list: {error}"))
        })?;

        Ok(list.into_models())
    }

    fn open_stream(&self, request: ProviderRequest) -> ProviderStream {
        let http = self.http.clone();
        let url = self.endpoint(ProtocolProfile::OPENCODE_GO.chat_path);
        let api_key = self.api_key.clone();
        let idle = self.stream_idle_timeout;
        let session = session_header(request.session_id.as_deref());
        let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);

        let task = tokio::spawn(async move {
            let headers = [
                ("User-Agent", USER_AGENT.to_owned()),
                (SESSION_HEADER, session),
            ];
            let result = match api_key {
                Some(api_key) => {
                    pump_stream(
                        &http,
                        &url,
                        &headers,
                        &api_key,
                        &request,
                        &ProtocolProfile::OPENCODE_GO,
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

#[async_trait]
impl Provider for OpenCodeGoClient {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.models().await
    }

    fn stream(&self, request: ProviderRequest) -> ProviderStream {
        self.open_stream(request)
    }
}

#[cfg(test)]
mod tests {
    use super::{SESSION_HEADER, session_header};

    #[test]
    fn a_session_header_is_stable_and_prefixed_once() {
        assert_eq!(session_header(Some("abc")), "ses_abc");
        assert_eq!(session_header(Some("ses_abc")), "ses_abc");
        assert_eq!(session_header(Some("abc")), session_header(Some("abc")));
        assert_eq!(SESSION_HEADER, "x-opencode-session");
    }

    #[test]
    fn a_missing_session_still_produces_a_value() {
        let value = session_header(None);

        assert!(value.starts_with("ses_"), "{value}");
        assert_eq!(value, session_header(Some("  ")));
    }
}
