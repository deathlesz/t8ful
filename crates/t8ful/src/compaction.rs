//! Model-backed context summarization.
//!
//! The selected model is used with tools disabled. Each chunk is bounded by an
//! estimate; every attempt and usage record is returned even when a later chunk
//! fails, so the journal can account for the work that ran.

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use t8ful_core::compaction::{
    CompactionError, CompactionLimits, CompactionOutput, ContextCompactor,
};
use t8ful_core::content::Content;
use t8ful_core::context::{estimate_message, estimate_tokens};
use t8ful_core::journal::{ModelAttempt, UsageRecord};
use t8ful_core::message::Message;
use t8ful_core::provider::{
    FinishReason, Provider, ProviderEvent, ProviderRequest, ReasoningEffort,
};
use t8ful_core::usage::Usage;
use tokio_util::sync::CancellationToken;

const SUMMARY_INSTRUCTION: &str = "\
Summarize this conversation history for another turn of the same coding agent. Preserve the \
user's constraints and decisions, files inspected or changed, commands and their important \
results, unresolved questions, and rejected approaches. Be concise but complete. Do not call \
tools and do not invent facts from the missing part of the conversation.";

pub struct ModelCompactor {
    provider: Arc<dyn Provider>,
}

impl ModelCompactor {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl ContextCompactor for ModelCompactor {
    async fn summarize(
        &self,
        prefix: &[Message],
        model: &str,
        agent: &str,
        request_prefix: &str,
        limits: CompactionLimits,
        cancel: &CancellationToken,
    ) -> Result<CompactionOutput, CompactionError> {
        let chunks = chunk_messages(prefix, limits.context_limit);
        let mut running = String::new();
        let mut attempts = Vec::new();
        let mut usages = Vec::new();

        for (index, chunk) in chunks.iter().enumerate() {
            let request_id = format!("{request_prefix}-chunk-{index}");
            let mut content = String::new();

            if !running.is_empty() {
                content.push_str("Previous summary:\n");
                content.push_str(&running);
                content.push_str("\n\nNew conversation history:\n");
            } else {
                content.push_str("Conversation history:\n");
            }

            for message in chunk {
                content.push_str(&view_message(message));
                content.push('\n');
            }

            let attempt = ModelAttempt {
                request_id: request_id.clone(),
                model: model.to_owned(),
                agent: agent.to_owned(),
                step: 0,
                max_output_tokens: limits.max_output_tokens,
                message_count: chunk.len(),
            };

            attempts.push(attempt);

            let request = ProviderRequest {
                model: model.to_owned(),
                system: SUMMARY_INSTRUCTION.to_owned(),
                messages: vec![Message::user_text(content)],
                tools: Vec::new(),
                max_output_tokens: limits.max_output_tokens,
                reasoning: ReasoningEffort::Default,
                reasoning_value: None,
            };
            let mut stream = self.provider.stream(request);
            let mut text = String::new();
            let mut finish = None;

            loop {
                let next = tokio::select! {
                    () = cancel.cancelled() => {
                        return Err(CompactionError::Cancelled);
                    }
                    next = stream.next() => next,
                };
                let Some(item) = next else {
                    break;
                };

                match item {
                    Ok(ProviderEvent::ReasoningDelta(_)) => {}
                    Ok(ProviderEvent::TextDelta(delta)) => text.push_str(&delta),
                    Ok(ProviderEvent::Usage(usage)) => {
                        usages.push(UsageRecord {
                            request_id: request_id.clone(),
                            model: model.to_owned(),
                            agent: agent.to_owned(),
                            kind: "compaction".to_owned(),
                            usage,
                        });
                    }
                    Ok(ProviderEvent::Finished(reason)) => finish = Some(reason),
                    Ok(ProviderEvent::ToolCallComplete(_)) => {
                        return Err(CompactionError::Failed(
                            "the summary model produced a tool call although tools were disabled"
                                .to_owned(),
                        ));
                    }
                    Err(error) => {
                        return Err(CompactionError::Failed(error.to_string()));
                    }
                }
            }

            if finish == Some(FinishReason::Length) {
                return Err(CompactionError::Truncated);
            }

            if text.trim().is_empty() {
                return Err(CompactionError::Failed(
                    "the summary model returned an empty summary".to_owned(),
                ));
            }

            running = text;
        }

        if running.trim().is_empty() {
            return Err(CompactionError::Failed(
                "no summary text was produced".to_owned(),
            ));
        }

        // A provider may omit usage. Keep an explicit empty Usage record so the
        // attempt is still visible in the journal and ledger.
        if usages.is_empty() {
            usages.push(UsageRecord {
                request_id: format!("{request_prefix}-chunk-0"),
                model: model.to_owned(),
                agent: agent.to_owned(),
                kind: "compaction".to_owned(),
                usage: Usage::default(),
            });
        }

        Ok(CompactionOutput {
            summary: running,
            chunks: chunks.len() as u32,
            usages,
            attempts,
        })
    }
}

fn chunk_messages(messages: &[Message], context_limit: u64) -> Vec<Vec<Message>> {
    let budget = context_limit.saturating_div(3).max(256);
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut used = estimate_tokens(SUMMARY_INSTRUCTION);

    for message in messages {
        let cost = estimate_message(message);

        if !current.is_empty() && used.saturating_add(cost) > budget {
            chunks.push(std::mem::take(&mut current));
            used = estimate_tokens(SUMMARY_INSTRUCTION);
        }

        current.push(message.clone());
        used = used.saturating_add(cost);
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    if chunks.is_empty() {
        chunks.push(Vec::new());
    }

    chunks
}

fn view_message(message: &Message) -> String {
    let mut rendered = String::new();

    for content in &message.content {
        match content {
            Content::Text { text } => {
                rendered.push_str(text);
                rendered.push('\n');
            }
            Content::Image(image) => {
                rendered.push_str(&format!(
                    "[image attachment {} {}x{}: {}]\n",
                    image.media_type,
                    image.width,
                    image.height,
                    image.path.display()
                ));
            }
            Content::ToolCall(call) => {
                rendered.push_str(&format!(
                    "[tool call {} {} {}]\n",
                    call.id, call.name, call.arguments
                ));
            }
            Content::ToolResult(result) => {
                rendered.push_str(&format!(
                    "[tool result {} {}: {}]\n",
                    result.call_id,
                    status_label(result.status),
                    result.content
                ));
            }
        }
    }

    rendered
}

fn status_label(status: t8ful_core::tool::ToolStatus) -> &'static str {
    match status {
        t8ful_core::tool::ToolStatus::Ok => "ok",
        t8ful_core::tool::ToolStatus::Error => "error",
        t8ful_core::tool::ToolStatus::Denied => "denied",
        t8ful_core::tool::ToolStatus::Indeterminate => "indeterminate",
        t8ful_core::tool::ToolStatus::NotExecuted => "not executed",
        t8ful_core::tool::ToolStatus::InputRequired => "input required",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use t8ful_core::error::ProviderError;
    use t8ful_core::provider::{
        FinishReason, ModelInfo, Provider, ProviderEvent, ProviderRequest, ProviderStream,
    };
    use t8ful_core::usage::Usage;
    use tokio_util::sync::CancellationToken;

    use super::ModelCompactor;
    use t8ful_core::compaction::{CompactionLimits, ContextCompactor};

    struct ScriptedProvider {
        steps: Mutex<VecDeque<Vec<Result<ProviderEvent, ProviderError>>>>,
        requests: Mutex<Vec<ProviderRequest>>,
    }

    impl ScriptedProvider {
        fn new(steps: Vec<Vec<Result<ProviderEvent, ProviderError>>>) -> Self {
            Self {
                steps: Mutex::new(steps.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }

        fn stream(&self, request: ProviderRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let step = self.steps.lock().unwrap().pop_front().unwrap_or_default();

            Box::pin(futures_util::stream::iter(step))
        }
    }

    fn prefix() -> Vec<t8ful_core::Message> {
        vec![
            t8ful_core::Message::user_text("read the config"),
            t8ful_core::Message::assistant(vec![t8ful_core::Content::text("I read it")]),
        ]
    }

    #[tokio::test]
    async fn summary_uses_the_selected_model_with_tools_disabled() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Ok(ProviderEvent::TextDelta("a compact summary".to_owned())),
            Ok(ProviderEvent::Usage(Usage {
                input_tokens: Some(40),
                output_tokens: Some(8),
                cost_usd: Some("0.0001".to_owned()),
                generation_id: Some(t8ful_core::GenerationId::new("gen-compact")),
                ..Usage::default()
            })),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]]));
        let compactor = ModelCompactor::new(provider.clone());
        let output = compactor
            .summarize(
                &prefix(),
                "vendor/model",
                "root",
                "session-1",
                CompactionLimits {
                    context_limit: 16_384,
                    max_output_tokens: Some(128),
                },
                &CancellationToken::new(),
            )
            .await
            .expect("summary");

        assert_eq!(output.summary, "a compact summary");
        assert_eq!(output.chunks, 1);
        assert_eq!(output.usages[0].kind, "compaction");
        assert_eq!(output.attempts.len(), 1);
        let requests = provider.requests.lock().unwrap();

        assert_eq!(requests[0].model, "vendor/model");
        assert!(requests[0].tools.is_empty());
        assert_eq!(requests[0].max_output_tokens, Some(128));
    }

    #[tokio::test]
    async fn truncated_or_empty_summaries_fail_without_a_projection() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Ok(ProviderEvent::TextDelta("partial".to_owned())),
            Ok(ProviderEvent::Finished(FinishReason::Length)),
        ]]));
        let compactor = ModelCompactor::new(provider);
        let error = compactor
            .summarize(
                &prefix(),
                "vendor/model",
                "root",
                "session-1",
                CompactionLimits {
                    context_limit: 16_384,
                    max_output_tokens: Some(8),
                },
                &CancellationToken::new(),
            )
            .await
            .expect_err("truncated");

        assert!(matches!(error, t8ful_core::CompactionError::Truncated));
    }

    #[tokio::test]
    async fn a_summary_that_calls_a_tool_is_rejected() {
        let call = t8ful_core::ToolCall::new(
            "call-1",
            "read_file",
            serde_json::json!({ "path": "secret" }),
        );
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Ok(ProviderEvent::ToolCallComplete(call)),
            Ok(ProviderEvent::Finished(FinishReason::ToolCalls)),
        ]]));
        let compactor = ModelCompactor::new(provider);
        let error = compactor
            .summarize(
                &prefix(),
                "vendor/model",
                "root",
                "session-1",
                CompactionLimits {
                    context_limit: 16_384,
                    max_output_tokens: Some(128),
                },
                &CancellationToken::new(),
            )
            .await
            .expect_err("tool call");

        assert!(
            error.to_string().contains("tool call"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn missing_usage_is_recorded_as_unknown() {
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            Ok(ProviderEvent::TextDelta("summary".to_owned())),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]]));
        let compactor = ModelCompactor::new(provider);
        let output = compactor
            .summarize(
                &prefix(),
                "vendor/model",
                "root",
                "session-1",
                CompactionLimits {
                    context_limit: 16_384,
                    max_output_tokens: None,
                },
                &CancellationToken::new(),
            )
            .await
            .expect("summary");

        assert_eq!(output.usages.len(), 1);
        assert!(output.usages[0].usage.input_tokens.is_none());
        assert!(output.usages[0].usage.cost_usd.is_none());
    }
}
