//! Reconstruction of complete provider events from OpenRouter stream chunks.

use std::collections::BTreeMap;

use serde::Deserialize;
use t8ful_core::ids::{CallId, GenerationId};
use t8ful_core::provider::{FinishReason, ProviderEvent};
use t8ful_core::tool::ToolCall;
use t8ful_core::usage::Usage;

use crate::sse::SseEvent;

/// Marker OpenRouter sends as the final data frame.
const DONE_MARKER: &str = "[DONE]";

/// Source of the fallback call id, unique for the whole process.
///
/// OpenRouter restarts the fragment `index` at zero for every response, so a
/// synthesized `call_{index}` named a fresh call `call_0` again on the next step
/// of the same turn. The agent loop keeps results per id and refuses a repeat,
/// so the second real call was reported as a replay of the first and never ran.
static NEXT_SYNTHESIZED_CALL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Accumulates streamed chunks into validated provider events.
///
/// Tool calls are reconstructed per `index` because OpenRouter fragments the
/// id, name, and JSON arguments across frames.
#[derive(Debug, Default)]
pub struct Assembler {
    calls: BTreeMap<u32, PartialCall>,
    finish: Option<FinishReason>,
    finished: bool,
    done: bool,
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Interpret one SSE frame.
    pub fn on_sse(
        &mut self,
        sse: &SseEvent,
    ) -> Result<Vec<ProviderEvent>, t8ful_core::ProviderError> {
        if sse.data.trim() == DONE_MARKER {
            self.done = true;

            return self.flush();
        }

        let chunk: Chunk = serde_json::from_str(&sse.data).map_err(|error| {
            t8ful_core::ProviderError::MalformedStream(format!("invalid chunk JSON: {error}"))
        })?;

        if let Some(error) = chunk.error {
            let status = error.status();
            let message = error
                .message
                .unwrap_or_else(|| "provider reported an unspecified error".to_owned());

            // The frame carries the same status a rejected request would, so
            // it gets the same classification. Frames without a usable status
            // stay unclassified rather than guessing one.
            return Err(match status {
                Some(status) => crate::client::status_error(status, message, None),
                None => t8ful_core::ProviderError::Other(format!(
                    "provider reported an error: {message}"
                )),
            });
        }

        let mut events = Vec::new();

        for choice in chunk.choices {
            // Reasoning is display-only. Emit it before content so a frontend
            // can keep the association even when both arrive in one frame.
            for reasoning in [
                choice.delta.reasoning.as_deref(),
                choice.delta.reasoning_content.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if !reasoning.is_empty() {
                    events.push(ProviderEvent::ReasoningDelta(reasoning.to_owned()));
                }
            }

            let content = choice
                .delta
                .content
                .as_ref()
                .map(ContentPayload::as_text)
                .unwrap_or_default();

            if !content.is_empty() {
                events.push(ProviderEvent::TextDelta(content));
            }

            for fragment in choice.delta.tool_calls {
                let index = fragment.index.unwrap_or(0);
                let call = self.calls.entry(index).or_default();

                if let Some(id) = fragment.id {
                    call.id.push_str(&id);
                }

                if let Some(function) = fragment.function {
                    if let Some(name) = function.name {
                        call.name.push_str(&name);
                    }

                    if let Some(arguments) = function.arguments {
                        call.arguments.push_str(&arguments);
                    }
                }
            }

            if let Some(reason) = choice.finish_reason {
                self.finish = Some(map_finish_reason(&reason));
                events.extend(self.flush()?);
            }
        }

        if let Some(usage) = chunk.usage {
            let usage = Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_read_tokens: usage.cache_read_tokens.or_else(|| {
                    usage
                        .prompt_tokens_details
                        .and_then(|details| details.cached_tokens)
                }),
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage
                    .completion_tokens_details
                    .and_then(|details| details.reasoning_tokens),
                cost_usd: usage.cost.or(usage.total_cost).map(|cost| cost.to_string()),
                generation_id: usage.generation_id.map(GenerationId::from),
            };

            if !usage.is_empty() {
                events.push(ProviderEvent::Usage(usage));
            }
        }

        Ok(events)
    }

    /// Finish the stream. Errors when the provider stopped without completing.
    pub fn finish(&mut self) -> Result<Vec<ProviderEvent>, t8ful_core::ProviderError> {
        if !self.done && !self.finished && self.finish.is_none() {
            return Err(t8ful_core::ProviderError::IncompleteStream(
                "the response ended before a finish reason or [DONE]".to_owned(),
            ));
        }

        self.flush()
    }

    /// Emit every accumulated tool call followed by the finish marker, once.
    ///
    /// A tool-call batch is delivered only when the provider explicitly says it
    /// finished to call tools. Anything else is an incomplete or contradictory
    /// response and must not reach dispatch.
    fn flush(&mut self) -> Result<Vec<ProviderEvent>, t8ful_core::ProviderError> {
        if self.finished {
            return Ok(Vec::new());
        }

        let calls = std::mem::take(&mut self.calls);

        if !calls.is_empty() {
            match self.finish {
                Some(FinishReason::ToolCalls) => {}
                Some(_) => {
                    return Err(t8ful_core::ProviderError::MalformedStream(
                        "the provider ended a tool-call batch with a non-tool finish reason"
                            .to_owned(),
                    ));
                }
                None => {
                    return Err(t8ful_core::ProviderError::MalformedStream(
                        "tool calls arrived without a tool_calls finish reason".to_owned(),
                    ));
                }
            }
        }

        let mut events = Vec::new();

        for (index, partial) in calls {
            events.push(ProviderEvent::ToolCallComplete(complete(index, partial)?));
        }

        if self.finish.is_some() || self.done {
            self.finished = true;

            events.push(ProviderEvent::Finished(
                self.finish.clone().unwrap_or(FinishReason::Stop),
            ));
        }

        Ok(events)
    }
}

fn complete(index: u32, partial: PartialCall) -> Result<ToolCall, t8ful_core::ProviderError> {
    if partial.name.trim().is_empty() {
        return Err(t8ful_core::ProviderError::MalformedToolCall {
            index,
            message: "the call has no function name".to_owned(),
        });
    }

    let raw_arguments = if partial.arguments.trim().is_empty() {
        "{}"
    } else {
        partial.arguments.as_str()
    };

    let arguments: serde_json::Value = serde_json::from_str(raw_arguments).map_err(|error| {
        t8ful_core::ProviderError::MalformedToolCall {
            index,
            message: format!("arguments are not valid JSON: {error}"),
        }
    })?;

    if !arguments.is_object() {
        return Err(t8ful_core::ProviderError::MalformedToolCall {
            index,
            message: "arguments must be a JSON object".to_owned(),
        });
    }

    let id = if partial.id.trim().is_empty() {
        format!(
            "call_{index}_{}",
            NEXT_SYNTHESIZED_CALL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    } else {
        partial.id
    };

    Ok(ToolCall {
        id: CallId::from(id),
        name: partial.name,
        arguments,
    })
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        other => FinishReason::Other(other.to_owned()),
    }
}

#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsagePayload>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    code: Option<serde_json::Value>,
}

impl ApiError {
    /// The HTTP status an in-band error frame reports, if it reports one.
    fn status(&self) -> Option<u16> {
        let status = match self.code.as_ref()? {
            serde_json::Value::Number(number) => number.as_u64()?,
            serde_json::Value::String(text) => text.trim().parse().ok()?,
            _ => return None,
        };

        u16::try_from(status)
            .ok()
            .filter(|status| (100..=599).contains(status))
    }
}

#[derive(Debug, Default, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<ContentPayload>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ContentPayload {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl ContentPayload {
    fn as_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|part| part.text.as_deref())
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ContentPart {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsagePayload {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    cache_read_tokens: Option<u64>,
    #[serde(default)]
    cache_write_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(default)]
    cost: Option<serde_json::Number>,
    #[serde(default)]
    total_cost: Option<serde_json::Number>,
    #[serde(default)]
    generation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use t8ful_core::provider::ProviderEvent;

    use super::Assembler;
    use crate::sse::SseEvent;

    fn event(data: serde_json::Value) -> SseEvent {
        SseEvent {
            event: None,
            data: data.to_string(),
        }
    }

    fn call_frame(with_id: bool, name: &str) -> SseEvent {
        let mut call = serde_json::json!({
            "index": 0,
            "function": { "name": name, "arguments": "{}" }
        });

        if with_id {
            call["id"] = serde_json::json!("call_given");
        }

        event(serde_json::json!({
            "choices": [{ "index": 0, "delta": { "tool_calls": [call] } }]
        }))
    }

    fn finish_frame() -> SseEvent {
        event(serde_json::json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
        }))
    }

    /// Collect the calls one response produced.
    fn calls_in_one_response(with_id: bool, name: &str) -> Vec<String> {
        let mut assembler = Assembler::new();
        let mut ids = Vec::new();

        for frame in [call_frame(with_id, name), finish_frame()] {
            for event in assembler.on_sse(&frame).expect("frame") {
                if let ProviderEvent::ToolCallComplete(call) = event {
                    ids.push(call.id.to_string());
                }
            }
        }

        ids
    }

    #[test]
    fn calls_without_a_provider_id_get_distinct_ids_per_response() {
        // Two steps of one turn: each response restarts the fragment index at
        // zero, and the agent loop refuses a repeated call id as a replay.
        let first = calls_in_one_response(false, "read_file");
        let second = calls_in_one_response(false, "read_file");

        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(second.len(), 1, "{second:?}");
        assert_ne!(
            first[0], second[0],
            "a later step reused the synthesized call id"
        );
    }

    #[test]
    fn a_provider_supplied_id_is_kept() {
        assert_eq!(calls_in_one_response(true, "read_file"), ["call_given"]);
    }

    #[test]
    fn an_in_band_error_frame_keeps_its_status_classification() {
        use t8ful_core::ProviderError;

        let error_for = |code: serde_json::Value| {
            let frame = event(serde_json::json!({
                "error": { "code": code, "message": "the key was rejected" },
                "choices": []
            }));

            Assembler::new()
                .on_sse(&frame)
                .expect_err("an error frame must not be ignored")
        };

        type Case = (serde_json::Value, fn(&ProviderError) -> bool, &'static str);

        let cases: Vec<Case> = vec![
            (
                serde_json::json!(401),
                |error| matches!(error, ProviderError::Auth(_)),
                "auth",
            ),
            (
                serde_json::json!(403),
                |error| matches!(error, ProviderError::Auth(_)),
                "auth",
            ),
            (
                serde_json::json!(404),
                |error| matches!(error, ProviderError::UnsupportedModel(_)),
                "unsupported model",
            ),
            (
                serde_json::json!(429),
                |error| matches!(error, ProviderError::RateLimited { .. }),
                "rate limit",
            ),
            (
                serde_json::json!(500),
                |error| matches!(error, ProviderError::Upstream { status: 500, .. }),
                "upstream",
            ),
            (
                serde_json::json!(null),
                |error| matches!(error, ProviderError::Other(_)),
                "unclassified",
            ),
        ];

        for (code, expected, label) in cases {
            let error = error_for(code.clone());

            assert!(
                expected(&error),
                "an error frame with code {code} should classify as {label}: {error:?}"
            );
            assert!(
                error.to_string().contains("the key was rejected"),
                "the provider message must survive: {error}"
            );
        }
    }
}
