//! HTTP plumbing shared by every OpenAI-compatible provider.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use t8ful_core::error::ProviderError;
use t8ful_core::provider::ProviderEvent;

/// Credential wrapper that never leaks its value through `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Build the HTTP client with an explicit root store.
///
/// The Mozilla root set ships with the binary, so a missing or unusual host
/// certificate store (a Nix build sandbox, a slim container) cannot break the
/// client at startup.
pub fn build_http_client() -> Result<reqwest::Client, ProviderError> {
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

/// Receiver half of a streamed completion tied to its HTTP pump.
///
/// Dropping the stream aborts the pump. Aborting is not a graceful close, but it
/// drops the in-flight request and socket, so an abandoned connection cannot
/// keep running or be mistaken for a completed response.
pub struct HttpStream {
    receiver: tokio::sync::mpsc::Receiver<Result<ProviderEvent, ProviderError>>,
    task: tokio::task::JoinHandle<()>,
}

impl HttpStream {
    /// Wrap the halves a provider's pump produces.
    pub fn new(
        receiver: tokio::sync::mpsc::Receiver<Result<ProviderEvent, ProviderError>>,
        task: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self { receiver, task }
    }
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

/// Read a successful response body through a hard byte cap.
///
/// A declared `Content-Length` over the limit is refused before reading; a
/// chunked body is stopped as soon as the running total crosses it.
pub async fn read_capped_body(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ProviderError> {
    use futures_util::StreamExt;

    if let Some(length) = response.content_length()
        && length > limit as u64
    {
        return Err(too_large(limit));
    }

    let mut stream = response.bytes_stream();
    let mut body = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(crate::error::transport_error)?;

        if body.len().saturating_add(chunk.len()) > limit {
            return Err(too_large(limit));
        }

        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

fn too_large(limit: usize) -> ProviderError {
    ProviderError::MalformedStream(format!(
        "the provider response is larger than the {limit}-byte limit"
    ))
}

#[cfg(test)]
mod tests {
    use super::Secret;

    #[test]
    fn credentials_never_appear_in_debug_output() {
        let secret = Secret::new("sk-or-very-secret");

        assert!(!format!("{secret:?}").contains("secret"));
        assert!(!format!("{secret}").contains("secret"));
        assert_eq!(secret.expose(), "sk-or-very-secret");
    }
}
