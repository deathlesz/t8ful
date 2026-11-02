use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use futures_core::Stream;
use futures_util::StreamExt;
use serde_json::{Value, json};
use t8ful_core::error::ProviderError;
use t8ful_core::message::{Message, Role};
use t8ful_core::provider::{
    ModelInfo, Provider, ProviderEvent, ProviderRequest, ProviderStream, ReasoningEffort,
};
use tokio::sync::mpsc;

use crate::models::ModelList;
use crate::sse::SseDecoder;
use crate::stream::Assembler;

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

const ERROR_BODY_LIMIT: usize = 512;
const STREAM_CHANNEL_CAPACITY: usize = 64;
/// Most bytes accepted for one model catalogue response.
pub const MAX_MODEL_CATALOG_BYTES: usize = 4 * 1024 * 1024;
/// Most bytes read from an error response before summarizing it.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// Deadline for reading an error response body.
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a chat request may go without sending a byte before it is abandoned.
///
/// A provider that accepts the connection and then stalls would otherwise hold
/// the turn open with no way back except cancelling it, so the wait for the
/// response and every later chunk are bounded separately from the catalog
/// deadline. A stream that keeps sending resets the deadline on each read.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Credential wrapper that never leaks its value through `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Redacted(String);

impl Redacted {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Redacted(***)")
    }
}

impl fmt::Display for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// OpenRouter HTTP client implementing the core provider contract.
pub struct OpenRouterClient {
    http: reqwest::Client,
    base_url: String,
    /// `None` when the client was built without a credential: every request
    /// then fails with [`ProviderError::MissingCredentials`] instead of sending
    /// an unauthenticated call.
    api_key: Option<Redacted>,
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
            api_key: Some(Redacted::new(api_key)),
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

    /// Replace the idle deadline, so a test can observe it without waiting.
    pub fn with_stream_idle_timeout(mut self, timeout: Duration) -> Self {
        self.stream_idle_timeout = timeout;
        self
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url)
    }

    fn authorized(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let Some(api_key) = &self.api_key else {
            return Err(ProviderError::MissingCredentials);
        };

        Ok(request
            .bearer_auth(api_key.expose())
            .header("X-Title", "t8ful"))
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let response = self
            .authorized(self.http.get(self.endpoint("models")))?
            .send()
            .await
            .map_err(transport_error)?;

        let response = check_response(response).await?;
        let body = read_capped_body(response, MAX_MODEL_CATALOG_BYTES).await?;
        let list: ModelList = serde_json::from_slice(&body).map_err(|error| {
            ProviderError::MalformedStream(format!("invalid model list: {error}"))
        })?;

        Ok(list.into_models())
    }

    fn open_stream(&self, request: ProviderRequest) -> ProviderStream {
        let http = self.http.clone();
        let url = self.endpoint("chat/completions");
        let api_key = self.api_key.clone();
        let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);

        let idle = self.stream_idle_timeout;

        let task = tokio::spawn(async move {
            let result = match api_key {
                Some(api_key) => pump_stream(http, url, api_key, request, idle, &sender).await,
                None => Err(ProviderError::MissingCredentials),
            };

            if let Err(error) = result {
                let _ = sender.send(Err(error)).await;
            }
        });

        Box::pin(HttpStream { receiver, task })
    }
}

/// Receiver half of a streamed completion tied to its HTTP pump.
///
/// Dropping the stream aborts the pump. Aborting is not a graceful close, but it
/// drops the in-flight request and socket, so an abandoned connection cannot
/// keep running or be mistaken for a completed response.
struct HttpStream {
    receiver: mpsc::Receiver<Result<ProviderEvent, ProviderError>>,
    task: tokio::task::JoinHandle<()>,
}

impl Stream for HttpStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(context)
    }
}

impl Drop for HttpStream {
    fn drop(&mut self) {
        self.task.abort();
    }
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

/// Build the HTTP client with an explicit root store.
///
/// The Mozilla root set ships with the binary, so a missing or unusual host
/// certificate store (a Nix build sandbox, a slim container) cannot break the
/// client at startup.
fn build_http_client() -> Result<reqwest::Client, ProviderError> {
    let mut roots = rustls::RootCertStore::empty();

    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .build()
        .map_err(|error| ProviderError::Transport(error.to_string()))
}

async fn pump_stream(
    http: reqwest::Client,
    url: String,
    api_key: Redacted,
    request: ProviderRequest,
    idle: Duration,
    sender: &mpsc::Sender<Result<ProviderEvent, ProviderError>>,
) -> Result<(), ProviderError> {
    let body = request_body(&request)?;

    let response = tokio::time::timeout(
        idle,
        http.post(&url)
            .bearer_auth(api_key.expose())
            .header("X-Title", "t8ful")
            .json(&body)
            .send(),
    )
    .await
    .map_err(|_| stalled(idle))?
    .map_err(transport_error)?;

    let response = check_response(response).await?;

    let mut decoder = SseDecoder::new();
    let mut assembler = Assembler::new();
    let mut bytes = response.bytes_stream();

    'stream: loop {
        let next = tokio::time::timeout(idle, bytes.next())
            .await
            .map_err(|_| stalled(idle))?;
        let Some(chunk) = next else {
            break 'stream;
        };
        let chunk = chunk.map_err(transport_error)?;

        for sse in decoder
            .push(&chunk)
            .map_err(|error| ProviderError::MalformedStream(error.to_string()))?
        {
            for event in assembler.on_sse(&sse)? {
                send_event(sender, event).await?;
            }

            if assembler.is_done() {
                break 'stream;
            }
        }
    }

    for sse in decoder
        .finish()
        .map_err(|error| ProviderError::MalformedStream(error.to_string()))?
    {
        for event in assembler.on_sse(&sse)? {
            send_event(sender, event).await?;
        }
    }

    for event in assembler.finish()? {
        send_event(sender, event).await?;
    }

    Ok(())
}

async fn send_event(
    sender: &mpsc::Sender<Result<ProviderEvent, ProviderError>>,
    event: ProviderEvent,
) -> Result<(), ProviderError> {
    sender
        .send(Ok(event))
        .await
        .map_err(|_| ProviderError::Cancelled)
}

fn request_body(request: &ProviderRequest) -> Result<Value, ProviderError> {
    let mut messages = Vec::new();

    if !request.system.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": request.system }));
    }

    for message in &request.messages {
        messages.extend(message_values(message)?);
    }

    let mut body = json!({
        "model": request.model,
        "stream": true,
        "messages": messages,
        "usage": { "include": true },
    });

    if !request.tools.is_empty() {
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                })
            })
            .collect::<Vec<_>>();

        body["tools"] = json!(tools);
    }

    if let Some(max_tokens) = request.max_output_tokens {
        body["max_tokens"] = json!(max_tokens);
    }

    match request.reasoning {
        ReasoningEffort::Default => {}
        ReasoningEffort::Off => body["reasoning"] = json!({ "enabled": false }),
        effort => {
            let value = request.reasoning_value.as_deref().unwrap_or(effort.label());

            body["reasoning"] = json!({ "effort": value });
        }
    }

    Ok(body)
}

fn message_values(message: &Message) -> Result<Vec<Value>, ProviderError> {
    Ok(match message.role {
        Role::System => vec![json!({ "role": "system", "content": message.text() })],
        Role::User => {
            if message.has_images() {
                let mut parts = Vec::new();
                let text = message.text();

                if !text.is_empty() {
                    parts.push(json!({ "type": "text", "text": text }));
                }

                for image in message.images() {
                    let bytes = std::fs::read(&image.path).map_err(|error| {
                        ProviderError::Other(format!(
                            "the image attachment {} is missing or unreadable: {error}; resume \
                             the session with its attachment directory intact or remove the \
                             attachment",
                            image.path.display()
                        ))
                    })?;
                    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);

                    parts.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{encoded}", image.media_type)
                        }
                    }));
                }

                vec![json!({ "role": "user", "content": parts })]
            } else {
                vec![json!({ "role": "user", "content": message.text() })]
            }
        }
        Role::Assistant => {
            let text = message.text();
            let calls = message
                .tool_calls()
                .map(|call| {
                    json!({
                        "id": call.id.as_str(),
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments.to_string(),
                        }
                    })
                })
                .collect::<Vec<_>>();

            let content = if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            };
            let mut value = json!({ "role": "assistant", "content": content });

            if !calls.is_empty() {
                value["tool_calls"] = json!(calls);
            }

            vec![value]
        }
        Role::Tool => message
            .results()
            .map(|result| {
                json!({
                    "role": "tool",
                    "tool_call_id": result.call_id.as_str(),
                    "content": result.content,
                })
            })
            .collect(),
    })
}

async fn check_response(response: reqwest::Response) -> Result<reqwest::Response, ProviderError> {
    let status = response.status();

    if status.is_success() {
        return Ok(response);
    }

    let retry_after_ms = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1000));

    let body = bounded_error_body(response).await;
    let message = summarize(&body);

    Err(status_error(status.as_u16(), message, retry_after_ms))
}

/// Read a successful response body through a hard byte cap.
///
/// A declared `Content-Length` over the limit is refused before reading; a
/// chunked body is stopped as soon as the running total crosses it.
async fn read_capped_body(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ProviderError> {
    if let Some(length) = response.content_length()
        && length > limit as u64
    {
        return Err(ProviderError::MalformedStream(format!(
            "the provider response is larger than the {limit}-byte limit"
        )));
    }

    let mut stream = response.bytes_stream();
    let mut body = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_error)?;

        if body.len().saturating_add(chunk.len()) > limit {
            return Err(ProviderError::MalformedStream(format!(
                "the provider response is larger than the {limit}-byte limit"
            )));
        }

        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// Read an error body through a byte cap and a short deadline.
///
/// The status and retry header are already known, so a huge or stalled error
/// body only costs a bounded wait and allocation before it is summarized.
async fn bounded_error_body(response: reqwest::Response) -> String {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();

    let read = async {
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                break;
            };

            let room = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);

            if body.len() >= MAX_ERROR_BODY_BYTES {
                break;
            }
        }
    };

    let _ = tokio::time::timeout(ERROR_BODY_TIMEOUT, read).await;

    String::from_utf8_lossy(&body).into_owned()
}

/// Classify an HTTP status the same way for a response and for an in-band
/// stream error frame, so both reach the user with the same meaning.
pub(crate) fn status_error(
    status: u16,
    message: String,
    retry_after_ms: Option<u64>,
) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth(message),
        404 => ProviderError::UnsupportedModel(message),
        429 => ProviderError::RateLimited {
            message,
            retry_after_ms,
        },
        other => ProviderError::Upstream {
            status: other,
            message,
        },
    }
}

/// The error for a request that went quiet for longer than its idle deadline.
fn stalled(idle: Duration) -> ProviderError {
    ProviderError::Transport(format!(
        "the provider sent nothing for {} ms",
        idle.as_millis()
    ))
}

fn transport_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        return ProviderError::Transport("the provider request timed out".to_owned());
    }

    ProviderError::Transport(error.to_string())
}

/// Collapse an error body into one bounded line so logs stay readable and do not
/// dump a whole HTML page.
fn summarize(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.is_empty() {
        return "the provider returned no error detail".to_owned();
    }

    if collapsed.chars().count() <= ERROR_BODY_LIMIT {
        return collapsed;
    }

    let truncated = collapsed.chars().take(ERROR_BODY_LIMIT).collect::<String>();

    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use t8ful_core::message::{Conversation, Message};
    use t8ful_core::provider::{Provider, ProviderRequest, ReasoningEffort};
    use t8ful_core::tool::{ToolCall, ToolResult, ToolSpec, ToolStatus};

    use super::{OpenRouterClient, Redacted, message_values, request_body, summarize};

    #[tokio::test]
    async fn an_unauthenticated_client_reports_missing_credentials() {
        let client = OpenRouterClient::unauthenticated("http://127.0.0.1:9").expect("client");
        let error = client.list_models().await.expect_err("missing credentials");

        assert!(
            matches!(error, t8ful_core::ProviderError::MissingCredentials),
            "{error}"
        );
        assert!(!format!("{client:?}").contains("sk-or"));
    }

    #[tokio::test]
    async fn an_unauthenticated_stream_reports_missing_credentials() {
        use futures_util::StreamExt;
        use t8ful_core::provider::{Provider, ProviderRequest};

        let client = OpenRouterClient::unauthenticated("http://127.0.0.1:9").expect("client");
        let request = ProviderRequest {
            model: "vendor/model".to_owned(),
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: None,
            reasoning: ReasoningEffort::Default,
            reasoning_value: None,
        };
        let mut stream = client.stream(request);
        let error = stream
            .next()
            .await
            .expect("one item")
            .expect_err("missing credentials");

        assert!(
            matches!(error, t8ful_core::ProviderError::MissingCredentials),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_optional_key_still_builds_a_client_without_one() {
        let client =
            OpenRouterClient::from_optional_api_key(None, "http://127.0.0.1:9").expect("client");
        let error = client.list_models().await.expect_err("missing credentials");

        assert!(matches!(
            error,
            t8ful_core::ProviderError::MissingCredentials
        ));
    }

    #[test]
    fn credentials_never_appear_in_debug_output() {
        let secret = Redacted::new("sk-or-very-secret");

        assert!(!format!("{secret:?}").contains("secret"));
        assert!(!format!("{secret}").contains("secret"));
        assert_eq!(secret.expose(), "sk-or-very-secret");
    }

    #[test]
    fn error_bodies_are_collapsed_and_bounded() {
        assert_eq!(summarize("a\n\nb   c"), "a b c");
        assert_eq!(summarize("   "), "the provider returned no error detail");

        let long = summarize(&"x".repeat(4096));

        assert!(long.chars().count() <= 513);
    }

    #[test]
    fn assistant_tool_calls_serialize_as_function_calls() {
        let message = Message::assistant(vec![
            t8ful_core::Content::text("thinking"),
            t8ful_core::Content::ToolCall(ToolCall::new(
                "call-1",
                "read_file",
                json!({ "path": "a.txt" }),
            )),
        ]);

        let values = message_values(&message).unwrap();

        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["role"], json!("assistant"));
        assert_eq!(values[0]["content"], json!("thinking"));
        assert_eq!(values[0]["tool_calls"][0]["id"], json!("call-1"));
        assert_eq!(
            values[0]["tool_calls"][0]["function"]["arguments"],
            json!("{\"path\":\"a.txt\"}")
        );
    }

    #[test]
    fn tool_results_become_one_message_each() {
        let call = ToolCall::new("call-1", "read_file", json!({}));
        let message = Message::tool_results([
            ToolResult::new(&call, ToolStatus::Ok, "first", false),
            ToolResult::new(&call, ToolStatus::Denied, "second", false),
        ]);

        let values = message_values(&message).unwrap();

        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["tool_call_id"], json!("call-1"));
        assert_eq!(values[1]["content"], json!("second"));
    }

    #[test]
    fn request_body_includes_tools_only_when_present() {
        let mut conversation = Conversation::new();

        conversation.push_user_text("hello");

        let request = ProviderRequest {
            model: "openrouter/test".to_owned(),
            system: "be brief".to_owned(),
            messages: conversation.messages().to_vec(),
            tools: Vec::new(),
            max_output_tokens: None,
            reasoning: ReasoningEffort::Default,
            reasoning_value: None,
        };

        let body = request_body(&request).unwrap();

        assert!(body.get("tools").is_none());
        assert!(body.get("reasoning").is_none());
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["model"], json!("openrouter/test"));
        assert_eq!(body["messages"][0]["role"], json!("system"));
        assert_eq!(body["messages"][0]["content"], json!("be brief"));
        assert_eq!(body["messages"][1]["role"], json!("user"));
    }

    #[test]
    fn request_body_serializes_tool_specs() {
        let request = ProviderRequest {
            model: "openrouter/test".to_owned(),
            system: String::new(),
            messages: vec![Message::user_text("go")],
            tools: vec![ToolSpec {
                name: "read_file".to_owned(),
                description: "reads".to_owned(),
                parameters: json!({ "type": "object" }),
            }],
            max_output_tokens: Some(64),
            reasoning: ReasoningEffort::Default,
            reasoning_value: None,
        };

        let body = request_body(&request).unwrap();

        assert_eq!(body["tools"][0]["function"]["name"], json!("read_file"));
        assert_eq!(body["max_tokens"], json!(64));
    }

    #[test]
    fn reasoning_effort_maps_to_the_openrouter_field() {
        let mut request = ProviderRequest {
            model: "openrouter/test".to_owned(),
            system: String::new(),
            messages: vec![Message::user_text("go")],
            tools: Vec::new(),
            max_output_tokens: None,
            reasoning: ReasoningEffort::Default,
            reasoning_value: None,
        };

        assert!(request_body(&request).unwrap().get("reasoning").is_none());

        request.reasoning = ReasoningEffort::Off;
        assert_eq!(
            request_body(&request).unwrap()["reasoning"],
            json!({ "enabled": false })
        );

        for (effort, expected) in [
            (ReasoningEffort::Minimal, "minimal"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::XHigh, "xhigh"),
            (ReasoningEffort::Max, "max"),
        ] {
            request.reasoning = effort;
            assert_eq!(
                request_body(&request).unwrap()["reasoning"],
                json!({ "effort": expected })
            );
        }
    }

    #[test]
    fn a_mapped_level_sends_the_provider_value() {
        let mut request = ProviderRequest {
            model: "openrouter/test".to_owned(),
            system: String::new(),
            messages: vec![Message::user_text("go")],
            tools: Vec::new(),
            max_output_tokens: None,
            reasoning: ReasoningEffort::High,
            reasoning_value: None,
        };

        request.reasoning_value = Some("adaptive".to_owned());

        assert_eq!(
            request_body(&request).unwrap()["reasoning"],
            json!({ "effort": "adaptive" })
        );

        request.reasoning = ReasoningEffort::Off;
        request.reasoning_value = Some("adaptive".to_owned());

        assert_eq!(
            request_body(&request).unwrap()["reasoning"],
            json!({ "enabled": false })
        );
    }
}
