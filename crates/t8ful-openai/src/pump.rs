//! The streamed chat request: send, decode, assemble, and forward.

use std::time::Duration;

use futures_util::StreamExt;
use t8ful_core::error::ProviderError;
use t8ful_core::provider::{ProviderEvent, ProviderRequest};
use tokio::sync::mpsc;

use crate::client::Secret;
use crate::error::{check_response, retry_delay, stalled, transport_error};
use crate::messages::request_body;
use crate::profile::ProtocolProfile;
use crate::sse::SseDecoder;
use crate::stream::Assembler;

/// Most bytes accepted for one model catalogue response.
pub const MAX_MODEL_CATALOG_BYTES: usize = 4 * 1024 * 1024;

/// How long a chat request may go without sending a byte before it is abandoned.
///
/// A provider that accepts the connection and then stalls would otherwise hold
/// the turn open with no way back except cancelling it, so the wait for the
/// response and every later chunk are bounded separately from the catalog
/// deadline. A stream that keeps sending resets the deadline on each read.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Send one chat completion and forward every assembled event to `sender`.
///
/// `headers` carries everything provider-specific except the bearer
/// authorization, which comes from `secret`. The profile decides the request
/// body's shape, so a provider never builds JSON itself.
#[allow(clippy::too_many_arguments)]
pub async fn pump_stream(
    http: &reqwest::Client,
    url: &str,
    headers: &[(&str, String)],
    secret: &Secret,
    request: &ProviderRequest,
    profile: &ProtocolProfile,
    idle: Duration,
    sender: &mpsc::Sender<Result<ProviderEvent, ProviderError>>,
) -> Result<(), ProviderError> {
    let body = request_body(request, profile)?;
    let response = send(http, url, headers, secret, &body, idle, sender).await?;

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

/// POST the request body, re-sending it while the provider keeps rejecting it
/// with a `Retry-After` the caller can afford to wait out.
///
/// Only the send is repeated. A rejection means the provider refused the request
/// before generating anything, so nothing was billed and no output exists to
/// duplicate. A failure partway through the stream is never retried: the pump
/// has already forwarded deltas to the consumer.
async fn send(
    http: &reqwest::Client,
    url: &str,
    headers: &[(&str, String)],
    secret: &Secret,
    body: &serde_json::Value,
    idle: Duration,
    sender: &mpsc::Sender<Result<ProviderEvent, ProviderError>>,
) -> Result<reqwest::Response, ProviderError> {
    let mut attempt = 0;

    loop {
        attempt += 1;

        // `send()` consumes the builder, so every attempt rebuilds it from the
        // one body, and no attempt can drift from the others.
        let mut builder = http.post(url).bearer_auth(secret.expose());

        for (name, value) in headers {
            builder = builder.header(*name, value);
        }

        let response = tokio::time::timeout(idle, builder.json(body).send())
            .await
            .map_err(|_| stalled(idle))?
            .map_err(transport_error)?;

        let error = match check_response(response).await {
            Ok(response) => return Ok(response),
            Err(error) => error,
        };
        let Some(delay) = retry_delay(&error, attempt) else {
            return Err(error);
        };

        send_event(
            sender,
            ProviderEvent::Warning(retry_warning(&error, attempt + 1, delay)),
        )
        .await?;
        wait(sender, delay).await?;
    }
}

/// Wait out one retry delay, or give up as soon as the consumer is gone.
async fn wait(
    sender: &mpsc::Sender<Result<ProviderEvent, ProviderError>>,
    delay: Duration,
) -> Result<(), ProviderError> {
    tokio::select! {
        () = sender.closed() => Err(ProviderError::Cancelled),
        () = tokio::time::sleep(delay) => Ok(()),
    }
}

/// One line naming the rejection, the attempt, and the wait.
fn retry_warning(error: &ProviderError, attempt: u32, delay: Duration) -> String {
    let reason = match error {
        ProviderError::RateLimited { .. } => "rate limited by the provider",
        ProviderError::PaymentRequired { .. } => {
            "the provider's in-flight payment budget is exhausted"
        }
        _ => "the provider rejected the request",
    };

    format!(
        "{reason}; retry {attempt}/{max} in {}",
        describe(delay),
        max = crate::error::MAX_ATTEMPTS
    )
}

fn describe(delay: Duration) -> String {
    if delay.subsec_millis() == 0 {
        format!("{}s", delay.as_secs())
    } else {
        format!("{}ms", delay.as_millis())
    }
}
