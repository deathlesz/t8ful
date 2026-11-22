//! Error classification shared by every OpenAI-compatible provider.

use std::time::Duration;

use futures_util::StreamExt;
use t8ful_core::error::ProviderError;

/// Longest error summary kept, in characters.
const ERROR_BODY_LIMIT: usize = 512;
/// Most bytes read from an error response before summarizing it.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// Deadline for reading an error response body.
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// Requests one chat completion may send, including the first one.
pub const MAX_ATTEMPTS: u32 = 5;
/// Longest single wait a retry will sit out.
///
/// A provider asking for more than this is telling the caller to come back
/// later, so reporting the error is more useful than holding the turn open.
pub const MAX_RETRY_WAIT: Duration = Duration::from_secs(120);

/// The OpenRouter limit source that clears on its own.
///
/// An in-flight budget is a concurrency allowance, so waiting out the header's
/// delay can succeed. Credits and key limits are only fixed by the user.
const IN_FLIGHT_BUDGET: &str = "openrouter_in_flight_budget";

/// Classify an HTTP status the same way for a response and for an in-band
/// stream error frame, so both reach the user with the same meaning.
pub fn status_error(status: u16, message: String, retry_after_ms: Option<u64>) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth(message),
        404 => ProviderError::UnsupportedModel(message),
        429 => ProviderError::RateLimited {
            message,
            retry_after_ms,
        },
        // An in-band frame carries no structured metadata, so the limit source
        // stays unknown and the error is never retried on a guess.
        402 => ProviderError::PaymentRequired {
            message,
            retry_after_ms,
            limit_source: None,
        },
        other => ProviderError::Upstream {
            status: other,
            message,
        },
    }
}

/// Turn a request's status into an error, reading a bounded error body first.
pub async fn check_response(
    response: reqwest::Response,
) -> Result<reqwest::Response, ProviderError> {
    let status = response.status();

    if status.is_success() {
        return Ok(response);
    }

    let retry_after_ms = retry_after_ms(response.headers());

    let body = bounded_error_body(response).await;

    Err(classify(status.as_u16(), &body, retry_after_ms))
}

/// The `Retry-After` delay in milliseconds, when the header holds one.
///
/// Only the delta-seconds form is read. An HTTP-date carries no advantage over
/// reporting the error and would need a clock comparison to interpret.
fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1000))
}

/// Classify a rejected request from its status and error body.
///
/// A payment refusal keeps the provider's structured `limit_source`, so a retry
/// decision can tell a limit that expires from one only the user can clear.
fn classify(status: u16, body: &str, retry_after_ms: Option<u64>) -> ProviderError {
    if status != 402 {
        return status_error(status, summarize(body), retry_after_ms);
    }

    let limit_source = limit_source(body);
    let message = match limit_source.as_deref() {
        Some(IN_FLIGHT_BUDGET) | None => summarize(body),
        Some(source) => format!(
            "{} (the `{source}` limit is exhausted; waiting cannot clear it, so add credits or \
             raise the limit and retry)",
            summarize(body)
        ),
    };

    ProviderError::PaymentRequired {
        message,
        retry_after_ms,
        limit_source,
    }
}

/// The provider's name for the limit behind a payment refusal, if it names one.
fn limit_source(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let source = parsed.get("error")?.get("metadata")?.get("limit_source")?;

    source
        .as_str()
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .map(str::to_owned)
}

/// How long to wait before re-sending a request the provider rejected.
///
/// `attempt` is the number of attempts already made, so the first rejection
/// arrives as `1`. A retry needs two things: an error class that was refused
/// before generation, and a header naming a wait short enough to sit out. Every
/// other case returns `None` and the error reaches the user unchanged.
pub fn retry_delay(error: &ProviderError, attempt: u32) -> Option<Duration> {
    if attempt >= MAX_ATTEMPTS {
        return None;
    }

    let retry_after_ms = match error {
        ProviderError::RateLimited { retry_after_ms, .. } => (*retry_after_ms)?,
        ProviderError::PaymentRequired {
            retry_after_ms,
            limit_source,
            ..
        } if limit_source.as_deref() == Some(IN_FLIGHT_BUDGET) => (*retry_after_ms)?,
        _ => return None,
    };
    let delay = Duration::from_millis(retry_after_ms);

    (delay <= MAX_RETRY_WAIT).then_some(delay)
}

/// Read an error body through a byte cap and a short deadline.
///
/// The status and retry header are already known, so a huge or stalled error
/// body only costs a bounded wait and allocation before it is summarized.
pub async fn bounded_error_body(response: reqwest::Response) -> String {
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

/// The error for a request that went quiet for longer than its idle deadline.
pub fn stalled(idle: Duration) -> ProviderError {
    ProviderError::Transport(format!(
        "the provider sent nothing for {} ms",
        idle.as_millis()
    ))
}

pub fn transport_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        return ProviderError::Transport("the provider request timed out".to_owned());
    }

    ProviderError::Transport(error.to_string())
}

/// Collapse an error body into one bounded line so logs stay readable and do not
/// dump a whole HTML page.
pub fn summarize(body: &str) -> String {
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
    use std::time::Duration;

    use t8ful_core::error::ProviderError;

    use super::{MAX_ATTEMPTS, MAX_RETRY_WAIT, classify, retry_delay, summarize};

    #[test]
    fn error_bodies_are_collapsed_and_bounded() {
        assert_eq!(summarize("a\n\nb   c"), "a b c");
        assert_eq!(summarize("   "), "the provider returned no error detail");

        let long = summarize(&"x".repeat(4096));

        assert!(long.chars().count() <= 513);
    }

    fn payment(message: &str, limit_source: Option<&str>) -> ProviderError {
        ProviderError::PaymentRequired {
            message: message.to_owned(),
            retry_after_ms: Some(1_000),
            limit_source: limit_source.map(str::to_owned),
        }
    }

    fn rate_limited(retry_after_ms: Option<u64>) -> ProviderError {
        ProviderError::RateLimited {
            message: "slow down".to_owned(),
            retry_after_ms,
        }
    }

    #[test]
    fn only_a_rejected_request_with_a_usable_header_is_retried() {
        type Case = (ProviderError, u32, Option<Duration>, &'static str);

        let cases: Vec<Case> = vec![
            (
                rate_limited(Some(1_000)),
                1,
                Some(Duration::from_secs(1)),
                "a 429 with a header waits",
            ),
            (
                rate_limited(None),
                1,
                None,
                "a 429 without a header is reported",
            ),
            (
                rate_limited(Some(1_000)),
                MAX_ATTEMPTS,
                None,
                "the attempt budget has run out",
            ),
            (
                payment("no budget", Some("openrouter_in_flight_budget")),
                1,
                Some(Duration::from_secs(1)),
                "an in-flight budget recovers on its own",
            ),
            (
                payment("no credits", Some("openrouter_credits")),
                1,
                None,
                "credits never recover by waiting",
            ),
            (
                payment("no budget", Some("openrouter_key_limit")),
                1,
                None,
                "a key limit never recovers by waiting",
            ),
            (
                payment("no budget", None),
                1,
                None,
                "an unknown limit source is not guessed",
            ),
            (
                rate_limited(Some(u64::MAX)),
                1,
                None,
                "an over-cap wait is reported instead of hanging",
            ),
            (
                ProviderError::Auth("bad key".to_owned()),
                1,
                None,
                "a rejected credential is not retried",
            ),
            (
                ProviderError::Upstream {
                    status: 500,
                    message: "boom".to_owned(),
                },
                1,
                None,
                "a 5xx has no retry arm",
            ),
        ];

        for (error, attempt, expected, label) in cases {
            assert_eq!(retry_delay(&error, attempt), expected, "{label}: {error:?}");
        }

        assert_eq!(MAX_RETRY_WAIT, Duration::from_secs(120));
    }

    #[test]
    fn a_payment_refusal_keeps_its_limit_source_and_reads_actionably() {
        let body = serde_json::json!({
            "error": {
                "code": 402,
                "message": "Insufficient credits",
                "metadata": { "limit_source": "openrouter_credits" },
            }
        })
        .to_string();

        match classify(402, &body, Some(0)) {
            ProviderError::PaymentRequired {
                limit_source,
                message,
                ..
            } => {
                assert_eq!(limit_source.as_deref(), Some("openrouter_credits"));
                assert!(message.contains("Insufficient credits"), "{message}");
                assert!(message.contains("waiting cannot clear it"), "{message}");
            }
            other => panic!("expected a payment refusal: {other:?}"),
        }

        // An in-flight budget keeps the provider's wording: it is retried, so
        // the warning says what is happening instead of the message.
        let in_flight = classify(
            402,
            &serde_json::json!({
                "error": {
                    "message": "in flight budget",
                    "metadata": { "limit_source": "openrouter_in_flight_budget" },
                }
            })
            .to_string(),
            Some(2_000),
        );

        match &in_flight {
            ProviderError::PaymentRequired {
                limit_source,
                message,
                retry_after_ms,
            } => {
                assert_eq!(limit_source.as_deref(), Some("openrouter_in_flight_budget"));
                assert!(message.contains("in flight budget"), "{message}");
                assert!(
                    !message.contains("waiting cannot clear it"),
                    "a retryable limit must not claim waiting is useless: {message}"
                );
                assert_eq!(*retry_after_ms, Some(2_000));
            }
            other => panic!("expected a payment refusal: {other:?}"),
        }

        // A body without metadata still classifies, without an actionable hint.
        let bare = classify(402, "no body detail", None);

        assert_eq!(
            bare.to_string(),
            "provider payment required: no body detail"
        );
    }
}
