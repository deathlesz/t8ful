use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use t8ful_core::content::Content;
use t8ful_core::error::{AgentError, ProviderError};
use t8ful_core::event::{AgentEvent, EventEmitter, SequencedEvent};
use t8ful_core::message::Conversation;
use t8ful_core::notice::{Notice, ProcessNotice};
use t8ful_core::policy::{Mode, Policy};
use t8ful_core::provider::{
    FinishReason, ModelInfo, Provider, ProviderEvent, ProviderRequest, ProviderStream,
};
use t8ful_core::tool::{
    Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolRegistry, ToolSpec,
    ToolStatus,
};
use t8ful_core::{Agent, AgentConfig};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Scripted provider
// ---------------------------------------------------------------------------

type Step = Vec<Result<ProviderEvent, ProviderError>>;

#[derive(Default)]
struct ScriptedProvider {
    steps: std::sync::Mutex<VecDeque<Step>>,
    requests: Arc<std::sync::Mutex<Vec<ProviderRequest>>>,
}

impl ScriptedProvider {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: std::sync::Mutex::new(steps.into()),
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Arc<std::sync::Mutex<Vec<ProviderRequest>>> {
        Arc::clone(&self.requests)
    }

    fn text_step(text: &str) -> Step {
        vec![
            Ok(ProviderEvent::TextDelta(text.to_owned())),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]
    }

    fn tool_step(calls: Vec<ToolCall>) -> Step {
        let mut step = calls
            .into_iter()
            .map(|call| Ok(ProviderEvent::ToolCallComplete(call)))
            .collect::<Vec<_>>();

        step.push(Ok(ProviderEvent::Finished(FinishReason::ToolCalls)));
        step
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    fn stream(&self, request: ProviderRequest) -> ProviderStream {
        self.requests.lock().expect("request log").push(request);

        let step = self
            .steps
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or_default();

        Box::pin(futures_util::stream::iter(step))
    }
}

// ---------------------------------------------------------------------------
// Counting tool
// ---------------------------------------------------------------------------

struct CountingTool {
    name: &'static str,
    effect: ToolEffect,
    calls: Arc<AtomicUsize>,
    delay: Duration,
    fail: bool,
}

impl CountingTool {
    fn new(name: &'static str, effect: ToolEffect, calls: Arc<AtomicUsize>) -> Self {
        Self {
            name,
            effect,
            calls,
            delay: Duration::ZERO,
            fail: false,
        }
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn failing(mut self) -> Self {
        self.fail = true;
        self
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.to_owned(),
            description: "counts invocations".to_owned(),
            parameters: json!({ "type": "object" }),
        }
    }

    fn effect(&self) -> ToolEffect {
        self.effect
    }

    async fn execute(&self, _call: &ToolCall, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }

        if self.fail {
            return Err(ToolError::Failed("tool exploded".to_owned()));
        }

        Ok(ToolOutput::text("tool ran"))
    }
}

/// A tool whose generic plan-mode message would mislead, so it explains itself.
struct ExplainingTool {
    effect: ToolEffect,
    reason: Option<&'static str>,
}

#[async_trait]
impl Tool for ExplainingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "explaining".to_owned(),
            description: "explains its denial".to_owned(),
            parameters: json!({ "type": "object" }),
        }
    }

    fn effect(&self) -> ToolEffect {
        self.effect
    }

    fn denial_reason(&self) -> Option<String> {
        self.reason.map(str::to_owned)
    }

    async fn execute(&self, _call: &ToolCall, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("ran"))
    }
}

/// Reports two display-only chunks before returning its real output.
struct ProgressTool;

#[async_trait]
impl Tool for ProgressTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "noisy".to_owned(),
            description: "streams progress".to_owned(),
            parameters: json!({ "type": "object" }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, _call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        ctx.report_progress(b"first chunk");
        ctx.report_progress(b"second chunk");

        Ok(ToolOutput::text("final output"))
    }
}

#[tokio::test]
async fn a_model_specific_reasoning_profile_drives_the_request() {
    use t8ful_core::provider::{ReasoningEffort, ReasoningProfile};

    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_step(
        "done",
    )]));
    let requests = provider.requests();
    let (emitter, _events) = EventEmitter::channel();
    let mut agent = Agent::new(
        provider,
        Arc::new(ToolRegistry::new()),
        Policy::new(Mode::Coding),
        AgentConfig::new("test/model", "system"),
        ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
    );
    // Only low and high exist here, and high maps to a provider-specific value.
    let profile = ReasoningProfile::from_support(Some(true))
        .with_mapped(ReasoningEffort::Minimal, None)
        .with_mapped(ReasoningEffort::Medium, None)
        .with_mapped(
            ReasoningEffort::High,
            Some("high-provider-value".to_owned()),
        );

    agent.set_reasoning_profile(profile);
    agent.set_reasoning_effort(ReasoningEffort::Medium);

    let mut conversation = Conversation::new();
    conversation.push_user_text("go");

    agent
        .run(&mut conversation, &emitter, &CancellationToken::new())
        .await
        .expect("turn");

    let requests = requests.lock().expect("requests");
    let request = requests.first().expect("request");

    // Medium is unsupported, so it clamps up to high and sends the mapped value.
    assert_eq!(request.reasoning, ReasoningEffort::High);
    assert_eq!(
        request.reasoning_value.as_deref(),
        Some("high-provider-value")
    );
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    agent: Agent,
    conversation: Conversation,
    events: mpsc::UnboundedReceiver<SequencedEvent>,
    emitter: EventEmitter,
    cancel: CancellationToken,
}

fn harness(provider: ScriptedProvider, tools: Vec<Box<dyn Tool>>) -> Harness {
    let (emitter, events) = EventEmitter::channel();

    let mut registry = ToolRegistry::new();

    for tool in tools {
        registry.register_arc(Arc::from(tool));
    }

    let context = ToolContext::new("/tmp/project".into(), "/tmp/project".into());
    let config = AgentConfig::new("test/model", "you are a test agent");

    let agent = Agent::new(
        Arc::new(provider),
        Arc::new(registry),
        Policy::new(Mode::Coding),
        config,
        context,
    );

    Harness {
        agent,
        conversation: Conversation::new(),
        events,
        emitter,
        cancel: CancellationToken::new(),
    }
}

impl Harness {
    async fn run(&mut self) -> Result<t8ful_core::TurnOutcome, AgentError> {
        self.agent
            .run(&mut self.conversation, &self.emitter, &self.cancel)
            .await
    }

    fn drain(&mut self) -> Vec<AgentEvent> {
        let mut collected = Vec::new();

        while let Ok(event) = self.events.try_recv() {
            collected.push(event.event);
        }

        collected
    }

    fn tool_results(&self) -> Vec<(String, ToolStatus, String)> {
        self.conversation
            .messages()
            .iter()
            .flat_map(|message| message.results())
            .map(|result| {
                (
                    result.call_id.as_str().to_owned(),
                    result.status,
                    result.content.clone(),
                )
            })
            .collect()
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall::new(id, name, json!({ "path": "src/main.rs" }))
}

fn bash_call(id: &str, command: &str) -> ToolCall {
    ToolCall::new(id, "bash", json!({ "command": command }))
}

// ---------------------------------------------------------------------------
// Agent loop behavior
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stream_ending_without_a_finish_reason_is_an_error() {
    let mut harness = harness(
        ScriptedProvider::new(vec![vec![Ok(ProviderEvent::TextDelta(
            "partial answer".to_owned(),
        ))]]),
        Vec::new(),
    );

    let error = harness
        .run()
        .await
        .expect_err("an incomplete stream must not look successful");

    assert!(
        matches!(
            error,
            AgentError::Provider(ProviderError::IncompleteStream(_))
        ),
        "{error}"
    );

    let events = harness.drain();

    assert!(
        !events.contains(&AgentEvent::TurnFinished {
            turn: 1,
            reason: "stop".to_owned()
        }),
        "{events:?}"
    );
}

#[tokio::test]
async fn reasoning_is_forwarded_before_text_and_never_enters_the_conversation() {
    let mut harness = harness(
        ScriptedProvider::new(vec![vec![
            Ok(ProviderEvent::ReasoningDelta("private thoughts".to_owned())),
            Ok(ProviderEvent::TextDelta("public answer".to_owned())),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]]),
        Vec::new(),
    );

    harness.run().await.expect("turn");

    let events = harness.drain();
    let reasoning = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ReasoningDelta { .. }))
        .expect("reasoning event");
    let text = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TextDelta { .. }))
        .expect("text event");

    assert!(reasoning < text, "{events:?}");
    assert!(
        harness
            .conversation
            .messages()
            .iter()
            .all(|message| !message.text().contains("private thoughts")),
        "{:?}",
        harness.conversation.messages()
    );
    assert!(
        harness
            .conversation
            .messages()
            .iter()
            .any(|message| message.text().contains("public answer"))
    );
}

#[tokio::test]
async fn reasoning_effort_is_stamped_on_every_request() {
    let provider = ScriptedProvider::new(vec![ScriptedProvider::text_step("answer")]);
    let requests = provider.requests();
    let mut harness = harness(provider, Vec::new());

    harness
        .agent
        .set_reasoning_effort(t8ful_core::provider::ReasoningEffort::High);
    harness.run().await.expect("turn");

    let requests = requests.lock().expect("request log");

    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].reasoning,
        t8ful_core::provider::ReasoningEffort::High
    );
}

#[test]
fn an_unsupported_model_resets_reasoning_with_a_warning() {
    let provider = ScriptedProvider::new(Vec::new());
    let mut harness = harness(provider, Vec::new());

    harness
        .agent
        .set_reasoning_effort(t8ful_core::provider::ReasoningEffort::Medium);

    let warning = harness
        .agent
        .set_model_profile("test/model", 8_000, 1_000, Some(true), Some(false))
        .expect("warning");

    assert!(
        warning.contains("does not advertise reasoning"),
        "{warning}"
    );
    assert_eq!(
        harness.agent.reasoning_effort(),
        t8ful_core::provider::ReasoningEffort::Default
    );

    harness
        .agent
        .set_reasoning_effort(t8ful_core::provider::ReasoningEffort::Low);

    assert!(
        harness
            .agent
            .set_model_profile("test/model", 8_000, 1_000, Some(true), Some(true))
            .is_none()
    );
    assert_eq!(
        harness.agent.reasoning_effort(),
        t8ful_core::provider::ReasoningEffort::Low
    );
}

#[tokio::test]
async fn text_only_turn_finishes_without_tools() {
    let mut harness = harness(
        ScriptedProvider::new(vec![
            ScriptedProvider::text_step("hello "),
            ScriptedProvider::text_step("world"),
        ]),
        Vec::new(),
    );

    let outcome = harness.run().await.expect("turn succeeds");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);
    assert_eq!(outcome.steps, 1);
    assert_eq!(harness.conversation.messages().len(), 1);
    assert_eq!(harness.conversation.messages()[0].text(), "hello ");

    let events = harness.drain();

    assert!(events.contains(&AgentEvent::TextCompleted {
        text: "hello ".to_owned()
    }));
    assert!(events.contains(&AgentEvent::TurnFinished {
        turn: 1,
        reason: "stop".to_owned()
    }));
}

#[tokio::test]
async fn fabricated_tool_markup_in_a_text_only_reply_is_reported_as_a_warning() {
    let reply = "<tool_result name=\"ask_user_question\">\n</tool_result>";
    let mut harness = harness(
        ScriptedProvider::new(vec![ScriptedProvider::text_step(reply)]),
        Vec::new(),
    );

    let outcome = harness.run().await.expect("turn succeeds");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);

    let events = harness.drain();

    assert!(
        events.contains(&AgentEvent::Warning {
            message: "the model wrote a `<tool_result>` block as text instead of calling a tool; \
                      nothing was dispatched"
                .to_owned(),
        }),
        "{events:?}"
    );
    assert_eq!(harness.conversation.messages().len(), 1);
    assert_eq!(harness.conversation.messages()[0].text(), reply);
    assert!(harness.tool_results().is_empty());
    assert!(events.contains(&AgentEvent::TurnFinished {
        turn: 1,
        reason: "stop".to_owned(),
    }));
}

#[tokio::test]
async fn a_tool_result_mentioned_inside_an_answer_is_not_a_fabrication() {
    let mut harness = harness(
        ScriptedProvider::new(vec![ScriptedProvider::text_step(
            "The model may write <tool_result> markup by mistake.",
        )]),
        Vec::new(),
    );

    harness.run().await.expect("turn succeeds");

    let events = harness.drain();

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Warning { .. })),
        "{events:?}"
    );
}

#[tokio::test]
async fn mutating_tool_inside_the_project_runs_without_prompting() {
    let calls = Arc::new(AtomicUsize::new(0));

    let mut harness = harness(
        ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![call("call-1", "edit")]),
            ScriptedProvider::text_step("done"),
        ]),
        vec![Box::new(CountingTool::new(
            "edit",
            ToolEffect::Mutating,
            Arc::clone(&calls),
        ))],
    );

    let outcome = harness.run().await.expect("turn succeeds");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);
    assert_eq!(outcome.steps, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    assert_eq!(harness.tool_results()[0].1, ToolStatus::Ok);
}

#[tokio::test]
async fn coding_mode_runs_a_dangerous_shell_command_without_prompting() {
    let calls = Arc::new(AtomicUsize::new(0));

    let mut harness = harness(
        ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![bash_call("call-1", "rm -rf /tmp/outside")]),
            ScriptedProvider::text_step("done"),
        ]),
        vec![Box::new(CountingTool::new(
            "bash",
            ToolEffect::Process,
            Arc::clone(&calls),
        ))],
    );

    harness.run().await.expect("turn succeeds");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(harness.tool_results()[0].1, ToolStatus::Ok);
}

#[tokio::test]
async fn plan_mode_denies_mutation_without_dispatch_and_continues() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = agent_with(
        ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![call("call-1", "edit")]),
            ScriptedProvider::text_step("the plan is ready"),
        ]),
        vec![Box::new(CountingTool::new(
            "edit",
            ToolEffect::Mutating,
            Arc::clone(&calls),
        ))],
        Mode::Plan,
    );
    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();
    let outcome = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("turn succeeds");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let results = conversation
        .messages()
        .iter()
        .flat_map(t8ful_core::Message::results)
        .collect::<Vec<_>>();

    assert_eq!(results[0].status, ToolStatus::Denied);
    assert!(
        results[0].content.contains("plan mode"),
        "{}",
        results[0].content
    );
}

#[tokio::test]
async fn read_only_tools_run_without_prompting() {
    let calls = Arc::new(AtomicUsize::new(0));

    let mut harness = harness(
        ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![call("call-1", "read_file")]),
            ScriptedProvider::text_step("done"),
        ]),
        vec![Box::new(CountingTool::new(
            "read_file",
            ToolEffect::ReadOnly,
            Arc::clone(&calls),
        ))],
    );

    harness.run().await.expect("turn succeeds");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unknown_tool_is_denied_without_dispatch() {
    let mut harness = harness(
        ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![call("call-1", "does_not_exist")]),
            ScriptedProvider::text_step("ok"),
        ]),
        Vec::new(),
    );

    harness.run().await.expect("turn succeeds");

    let results = harness.tool_results();

    assert_eq!(results[0].1, ToolStatus::Denied);
    assert!(results[0].2.contains("unknown tool"));
}

#[tokio::test]
async fn failing_tool_reports_an_error_result_and_keeps_the_turn_alive() {
    let calls = Arc::new(AtomicUsize::new(0));

    let mut harness = harness(
        ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![call("call-1", "shell")]),
            ScriptedProvider::text_step("recovered"),
        ]),
        vec![Box::new(
            CountingTool::new("shell", ToolEffect::Process, Arc::clone(&calls)).failing(),
        )],
    );

    let outcome = harness.run().await.expect("turn succeeds");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);
    assert_eq!(harness.tool_results()[0].1, ToolStatus::Error);
    assert_eq!(
        harness.conversation.messages().last().unwrap().text(),
        "recovered"
    );
}

/// Run one tool call in plan mode and return the recorded denial.
async fn plan_mode_denial(tool: Box<dyn Tool>) -> (ToolStatus, String) {
    let provider = ScriptedProvider::new(vec![
        ScriptedProvider::tool_step(vec![call("call-1", "explaining")]),
        ScriptedProvider::text_step("understood"),
    ]);
    let agent = agent_with(provider, vec![tool], Mode::Plan);
    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("the turn succeeds");

    let result = conversation
        .messages()
        .iter()
        .flat_map(t8ful_core::Message::results)
        .next()
        .expect("one result");

    (result.status, result.content.clone())
}

#[tokio::test]
async fn a_tool_specific_denial_reason_replaces_the_generic_one() {
    let (status, content) = plan_mode_denial(Box::new(ExplainingTool {
        effect: ToolEffect::Mutating,
        reason: Some("server `github` is not trusted for plan mode"),
    }))
    .await;

    assert_eq!(status, ToolStatus::Denied);
    assert!(content.contains("not trusted for plan mode"), "{content}");
    assert!(
        !content.contains("present a plan and leave plan mode"),
        "the generic message must be replaced: {content}"
    );
}

#[tokio::test]
async fn a_tool_without_a_reason_keeps_the_generic_denial() {
    let (status, content) = plan_mode_denial(Box::new(ExplainingTool {
        effect: ToolEffect::Mutating,
        reason: None,
    }))
    .await;

    assert_eq!(status, ToolStatus::Denied);
    assert!(
        content.contains("present a plan and leave plan mode"),
        "{content}"
    );
}

#[tokio::test]
async fn cancelled_tool_is_indeterminate_and_never_replayed() {
    let calls = Arc::new(AtomicUsize::new(0));

    let mut harness = harness(
        ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![call("call-1", "shell")]),
            // The second run of the same conversation receives the same call.
            ScriptedProvider::tool_step(vec![call("call-1", "shell")]),
            ScriptedProvider::text_step("done"),
        ]),
        vec![Box::new(
            CountingTool::new("shell", ToolEffect::Process, Arc::clone(&calls))
                .with_delay(Duration::from_secs(30)),
        )],
    );

    let canceller = harness.cancel.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        canceller.cancel();
    });

    let error = harness.run().await.expect_err("turn is cancelled");

    assert!(matches!(error, AgentError::Cancelled));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(harness.tool_results()[0].1, ToolStatus::Indeterminate);

    // A fresh turn over the same conversation must not repeat the action.
    harness.cancel = CancellationToken::new();

    let outcome = harness.run().await.expect("second turn succeeds");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let results = harness.tool_results();

    assert_eq!(results.len(), 2);
    assert_eq!(results[1].1, ToolStatus::Error);
    assert!(results[1].2.contains("refusing to run it twice"));
}

#[tokio::test]
async fn tool_batch_duplicate_ids_are_rejected_before_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let step = vec![
        Ok(ProviderEvent::ToolCallComplete(call("call-1", "edit"))),
        Ok(ProviderEvent::ToolCallComplete(call("call-1", "edit"))),
        Ok(ProviderEvent::Finished(FinishReason::ToolCalls)),
    ];
    let mut harness = harness(
        ScriptedProvider::new(vec![step]),
        vec![Box::new(CountingTool::new(
            "edit",
            ToolEffect::Mutating,
            Arc::clone(&calls),
        ))],
    );

    let error = harness
        .run()
        .await
        .expect_err("duplicate batch is rejected");

    assert!(matches!(error, AgentError::Provider(_)), "{error:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(harness.conversation.messages().is_empty());
    assert!(harness.conversation.unanswered_tool_calls().is_empty());

    let events = harness.drain();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Error { .. })),
        "events: {events:?}"
    );
    assert!(events.contains(&AgentEvent::TurnFinished {
        turn: 1,
        reason: "error".to_owned()
    }));
}

#[tokio::test]
async fn tool_batch_without_a_finish_reason_is_rejected_before_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let step = vec![Ok(ProviderEvent::ToolCallComplete(call("call-1", "edit")))];
    let mut harness = harness(
        ScriptedProvider::new(vec![step]),
        vec![Box::new(CountingTool::new(
            "edit",
            ToolEffect::Mutating,
            Arc::clone(&calls),
        ))],
    );

    let error = harness
        .run()
        .await
        .expect_err("unfinished batch is rejected");

    assert!(matches!(error, AgentError::Provider(_)), "{error:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(harness.conversation.messages().is_empty());
    assert!(harness.conversation.unanswered_tool_calls().is_empty());
}

#[tokio::test]
async fn tool_batch_with_a_non_tool_finish_is_rejected_before_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let step = vec![
        Ok(ProviderEvent::ToolCallComplete(call("call-1", "edit"))),
        Ok(ProviderEvent::Finished(FinishReason::Length)),
    ];
    let mut harness = harness(
        ScriptedProvider::new(vec![step]),
        vec![Box::new(CountingTool::new(
            "edit",
            ToolEffect::Mutating,
            Arc::clone(&calls),
        ))],
    );

    let error = harness.run().await.expect_err("length batch is rejected");

    assert!(matches!(error, AgentError::Provider(_)), "{error:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(harness.conversation.messages().is_empty());
    assert!(harness.conversation.unanswered_tool_calls().is_empty());
}

#[tokio::test]
async fn tool_batch_with_unique_ids_dispatches_each_call_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut harness = harness(
        ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![
                call("call-1", "read_file"),
                call("call-2", "read_file"),
            ]),
            ScriptedProvider::text_step("done"),
        ]),
        vec![Box::new(CountingTool::new(
            "read_file",
            ToolEffect::ReadOnly,
            Arc::clone(&calls),
        ))],
    );

    harness.run().await.expect("valid batch runs");

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(harness.tool_results().len(), 2);
    assert!(harness.conversation.unanswered_tool_calls().is_empty());
}

#[tokio::test]
async fn max_steps_stops_a_runaway_loop() {
    let calls = Arc::new(AtomicUsize::new(0));

    let steps = (0..3)
        .map(|index| ScriptedProvider::tool_step(vec![call(&format!("call-{index}"), "read_file")]))
        .collect::<Vec<_>>();

    let (emitter, mut events) = EventEmitter::channel();
    let mut registry = ToolRegistry::new();

    registry.register(CountingTool::new(
        "read_file",
        ToolEffect::ReadOnly,
        Arc::clone(&calls),
    ));

    let mut config = AgentConfig::new("test/model", "you are a test agent");

    config.max_steps = Some(3);

    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(steps)),
        Arc::new(registry),
        Policy::new(Mode::Coding),
        config,
        ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
    );

    let mut conversation = Conversation::new();
    let cancel = CancellationToken::new();
    let outcome = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("turn stops at the step limit");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::MaxSteps);
    assert_eq!(outcome.steps, 3);
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    let mut seen = Vec::new();

    while let Ok(event) = events.try_recv() {
        seen.push(event.event);
    }

    assert!(seen.contains(&AgentEvent::TurnFinished {
        turn: 1,
        reason: "max_steps".to_owned()
    }));
}

#[tokio::test]
async fn an_unbounded_turn_runs_past_the_old_default_step_limit() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut steps = (0..40)
        .map(|index| ScriptedProvider::tool_step(vec![call(&format!("call-{index}"), "read_file")]))
        .collect::<Vec<_>>();

    steps.push(ScriptedProvider::text_step("done"));

    let (emitter, _events) = EventEmitter::channel();
    let mut registry = ToolRegistry::new();

    registry.register(CountingTool::new(
        "read_file",
        ToolEffect::ReadOnly,
        Arc::clone(&calls),
    ));

    let config = AgentConfig::new("test/model", "you are a test agent");

    assert_eq!(config.max_steps, None);

    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(steps)),
        Arc::new(registry),
        Policy::new(Mode::Coding),
        config,
        ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
    );

    let mut conversation = Conversation::new();
    let cancel = CancellationToken::new();
    let outcome = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("unbounded turn finishes with the model's stop reason");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);
    assert_eq!(outcome.steps, 41);
    assert_eq!(calls.load(Ordering::SeqCst), 40);
}

#[tokio::test]
async fn usage_carries_cache_and_reasoning_counts() {
    let step = vec![
        Ok(ProviderEvent::Usage(t8ful_core::Usage {
            input_tokens: Some(120),
            output_tokens: Some(40),
            cache_read_tokens: Some(60),
            cache_write_tokens: Some(20),
            reasoning_tokens: Some(15),
            ..t8ful_core::Usage::default()
        })),
        Ok(ProviderEvent::TextDelta("hi".to_owned())),
        Ok(ProviderEvent::Finished(FinishReason::Stop)),
    ];

    let mut harness = harness(ScriptedProvider::new(vec![step]), Vec::new());

    harness.run().await.expect("turn succeeds");

    let events = harness.drain();
    let usage = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::UsageRecorded {
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                ..
            } => Some((*cache_read_tokens, *cache_write_tokens, *reasoning_tokens)),
            _ => None,
        })
        .expect("usage event");

    assert_eq!(usage, (Some(60), Some(20), Some(15)));
}

#[tokio::test]
async fn a_process_notice_is_journaled_once_and_announced() {
    let mut harness = harness(ScriptedProvider::new(Vec::new()), Vec::new());
    let notice = Notice::Process(ProcessNotice {
        process_id: "process-1".to_owned(),
        command: "cargo watch".to_owned(),
        state: "exited".to_owned(),
        exit_code: Some(0),
        signal: None,
        elapsed_ms: 1200,
        total_bytes: 12,
        truncated: false,
        output_tail: "done".to_owned(),
    });

    harness
        .agent
        .deliver_notice(notice, &mut harness.conversation, &harness.emitter)
        .await
        .expect("delivered");

    let text = harness
        .conversation
        .messages()
        .last()
        .expect("notice message")
        .text();

    assert!(text.contains("process-1"), "{text}");
    assert!(text.contains("cargo watch"), "{text}");

    let events = harness.drain();
    let delivered = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ProcessNoticeDelivered { process_id } if process_id == "process-1"))
        .count();

    assert_eq!(delivered, 1);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::AgentNoticeDelivered { .. }))
    );
}

#[tokio::test]
async fn usage_is_forwarded_without_inventing_missing_values() {
    let step = vec![
        Ok(ProviderEvent::Usage(t8ful_core::Usage {
            input_tokens: Some(12),
            output_tokens: None,
            cost_usd: Some("0.00042".to_owned()),
            generation_id: None,
            ..t8ful_core::Usage::default()
        })),
        Ok(ProviderEvent::TextDelta("hi".to_owned())),
        Ok(ProviderEvent::Finished(FinishReason::Stop)),
    ];

    let mut harness = harness(ScriptedProvider::new(vec![step]), Vec::new());

    harness.run().await.expect("turn succeeds");

    let events = harness.drain();
    let usage = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::UsageRecorded {
                input_tokens,
                output_tokens,
                cost_usd,
                ..
            } => Some((*input_tokens, *output_tokens, cost_usd.clone())),
            _ => None,
        })
        .expect("usage event");

    assert_eq!(usage, (Some(12), None, Some("0.00042".to_owned())));
}

#[tokio::test]
async fn conversation_tracks_unanswered_calls() {
    let mut conversation = Conversation::new();

    conversation.push(t8ful_core::Message::assistant(vec![Content::ToolCall(
        call("call-1", "read_file"),
    )]));

    assert_eq!(conversation.unanswered_tool_calls().len(), 1);
    assert!(!conversation.has_result_for("call-1"));

    conversation.push(t8ful_core::Message::tool_results([
        t8ful_core::ToolResult::new(
            &call("call-1", "read_file"),
            ToolStatus::Ok,
            "contents",
            false,
        ),
    ]));

    assert!(conversation.unanswered_tool_calls().is_empty());
    assert!(conversation.has_result_for("call-1"));
}

// ---------------------------------------------------------------------------
// Steering, mode changes, and compaction
// ---------------------------------------------------------------------------

struct FixedCompactor {
    outcome: Result<t8ful_core::CompactionOutput, t8ful_core::CompactionError>,
    called: Arc<AtomicUsize>,
    prefix_len: Arc<AtomicUsize>,
}

#[async_trait]
impl t8ful_core::ContextCompactor for FixedCompactor {
    async fn summarize(
        &self,
        prefix: &[t8ful_core::Message],
        _model: &str,
        _agent: &str,
        _request_prefix: &str,
        _limits: t8ful_core::compaction::CompactionLimits,
        _cancel: &CancellationToken,
    ) -> Result<t8ful_core::CompactionOutput, t8ful_core::CompactionError> {
        self.called.fetch_add(1, Ordering::SeqCst);
        self.prefix_len.store(prefix.len(), Ordering::SeqCst);

        self.outcome.clone()
    }
}

const TEST_CONTEXT_LIMIT: u64 = 40_000;

fn push_large_turn(conversation: &mut Conversation, index: usize) {
    conversation.push_user_text(format!("turn {index}\n{}", "x".repeat(5_000)));
    conversation.push(t8ful_core::Message::assistant(vec![Content::text(
        "y".repeat(5_000),
    )]));
}

fn compactable_conversation(turns: usize) -> Conversation {
    let mut conversation = Conversation::new();

    for index in 0..turns {
        push_large_turn(&mut conversation, index);
    }

    conversation
}

fn split_turn_conversation() -> Conversation {
    let mut conversation = compactable_conversation(5);

    conversation.push_user_text(format!("final request\n{}", "u".repeat(3_000)));
    conversation.push(t8ful_core::Message::assistant(vec![Content::text(
        "z".repeat(100_000),
    )]));

    conversation
}

fn agent_with(provider: ScriptedProvider, tools: Vec<Box<dyn Tool>>, mode: Mode) -> Agent {
    let mut registry = ToolRegistry::new();

    for tool in tools {
        registry.register_arc(Arc::from(tool));
    }

    Agent::new(
        Arc::new(provider),
        Arc::new(registry),
        Policy::new(mode),
        AgentConfig::new("test/model", "you are a test agent"),
        ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
    )
}

#[tokio::test]
async fn steering_never_cancels_calls_and_is_delivered_before_the_next_model_step() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider::new(vec![
        ScriptedProvider::tool_step(vec![call("call-1", "slow"), call("call-2", "slow")]),
        ScriptedProvider::text_step("after steering"),
    ]);
    let requests = provider.requests();
    let agent = agent_with(
        provider,
        vec![Box::new(
            CountingTool::new("slow", ToolEffect::ReadOnly, Arc::clone(&calls))
                .with_delay(Duration::from_millis(30)),
        )],
        t8ful_core::Mode::Coding,
    );
    let (emitter, mut events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();
    let run = agent.run(&mut conversation, &emitter, &cancel);
    let steer = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        agent.steer("please change direction");
    };

    let (outcome, ()) = tokio::join!(run, steer);
    let outcome = outcome.expect("turn succeeds");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a steer must not cancel the current step's calls"
    );

    let results = conversation
        .messages()
        .iter()
        .flat_map(t8ful_core::Message::results)
        .collect::<Vec<_>>();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].status, ToolStatus::Ok);
    assert_eq!(results[1].status, ToolStatus::Ok);

    let requests = requests.lock().expect("request log");

    assert_eq!(
        requests.len(),
        2,
        "the steer should start one more model step"
    );

    let messages = &requests[1].messages;
    let steer_index = messages
        .iter()
        .position(|message| {
            message.role == t8ful_core::Role::User && message.text().contains("change direction")
        })
        .expect("the steer is in the next request");
    let results_index = messages
        .iter()
        .rposition(|message| message.results().next().is_some())
        .expect("the tool results are in the next request");

    assert!(
        results_index < steer_index,
        "the steer must be delivered after the tool batch: {messages:#?}"
    );

    let events = std::iter::from_fn(|| events.try_recv().ok())
        .map(|event| event.event)
        .collect::<Vec<_>>();

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnFinished { reason, .. } if reason == "stop"
    )));
}

#[tokio::test]
async fn delivered_steering_emits_one_user_message_and_skips_empty_wake_ups() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = agent_with(
        ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![call("call-1", "slow"), call("call-2", "slow")]),
            ScriptedProvider::text_step("after steering"),
        ]),
        vec![Box::new(
            CountingTool::new("slow", ToolEffect::ReadOnly, Arc::clone(&calls))
                .with_delay(Duration::from_millis(40)),
        )],
        t8ful_core::Mode::Coding,
    );
    let (emitter, mut events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();
    let run = agent.run(&mut conversation, &emitter, &cancel);
    let steer = async {
        tokio::time::sleep(Duration::from_millis(5)).await;
        agent.steer("first steer");
        agent.steer("");
        agent.steer("second steer");
    };

    let (outcome, ()) = tokio::join!(run, steer);

    outcome.expect("turn succeeds");

    let delivered = std::iter::from_fn(|| events.try_recv().ok())
        .map(|event| event.event)
        .filter_map(|event| match event {
            AgentEvent::UserMessage { text, source, .. } => Some((text, source)),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(
        delivered,
        vec![
            (
                "first steer".to_owned(),
                t8ful_core::event::UserMessageSource::Steered
            ),
            (
                "second steer".to_owned(),
                t8ful_core::event::UserMessageSource::Steered
            ),
        ]
    );

    let user_messages = conversation
        .messages()
        .iter()
        .filter(|message| message.role == t8ful_core::Role::User)
        .count();

    assert_eq!(user_messages, 2, "the empty wake-up steer is not a message");
}

#[tokio::test]
async fn drained_steering_is_never_delivered() {
    let agent = agent_with(
        ScriptedProvider::new(vec![ScriptedProvider::text_step("done")]),
        vec![],
        t8ful_core::Mode::Coding,
    );

    agent.steer("recalled by the user");

    assert_eq!(
        agent.take_pending_steering(),
        vec!["recalled by the user".to_owned()]
    );

    let (emitter, mut events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("turn succeeds");

    let delivered = std::iter::from_fn(|| events.try_recv().ok())
        .map(|event| event.event)
        .filter(|event| {
            matches!(
                event,
                AgentEvent::UserMessage {
                    source: t8ful_core::event::UserMessageSource::Steered,
                    ..
                }
            )
        })
        .count();

    assert_eq!(delivered, 0);
    assert!(
        conversation
            .messages()
            .iter()
            .all(|message| !message.text().contains("recalled by the user"))
    );
}

#[tokio::test]
async fn tool_progress_is_forwarded_with_the_call_id_and_never_replaces_the_result() {
    let agent = agent_with(
        ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![call("call-1", "noisy")]),
            ScriptedProvider::text_step("done"),
        ]),
        vec![Box::new(ProgressTool)],
        t8ful_core::Mode::Coding,
    );
    let (emitter, mut events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("turn succeeds");

    let events = std::iter::from_fn(|| events.try_recv().ok())
        .map(|event| event.event)
        .collect::<Vec<_>>();
    let progress = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolProgress {
                call_id,
                chunk,
                truncated,
            } => Some((call_id.as_str(), chunk.as_str(), *truncated)),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(
        progress,
        vec![
            ("call-1", "first chunk", false),
            ("call-1", "second chunk", false)
        ]
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResult { content, .. } if content == "final output"
    )));
    assert_eq!(
        conversation
            .messages()
            .iter()
            .flat_map(t8ful_core::Message::results)
            .count(),
        1
    );
}

#[tokio::test]
async fn progress_stops_at_the_output_budget_and_announces_truncation() {
    let mut context = ToolContext::new("/tmp/project".into(), "/tmp/project".into());

    context.max_output_bytes = 4;

    let (emitter, mut events) = EventEmitter::channel();
    let mut conversation = Conversation::new();
    let mut registry = ToolRegistry::new();

    registry.register(ProgressTool);

    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![call("call-1", "noisy")]),
            ScriptedProvider::text_step("done"),
        ])),
        Arc::new(registry),
        Policy::new(t8ful_core::Mode::Coding),
        AgentConfig::new("test/model", "you are a test agent"),
        context,
    );
    let cancel = CancellationToken::new();

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("turn succeeds");

    let progress = std::iter::from_fn(|| events.try_recv().ok())
        .map(|event| event.event)
        .filter_map(|event| match event {
            AgentEvent::ToolProgress {
                chunk, truncated, ..
            } => Some((chunk, truncated)),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(
        progress,
        vec![("firs".to_owned(), true)],
        "the cap keeps the first bytes and reports the cut once"
    );
}

#[tokio::test]
async fn mode_change_stops_before_dispatching_unstarted_calls() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = agent_with(
        ScriptedProvider::new(vec![ScriptedProvider::tool_step(vec![
            call("call-1", "slow"),
            call("call-2", "slow"),
        ])]),
        vec![Box::new(
            CountingTool::new("slow", ToolEffect::ReadOnly, Arc::clone(&calls))
                .with_delay(Duration::from_millis(30)),
        )],
        t8ful_core::Mode::Coding,
    );
    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();
    let run = agent.run(&mut conversation, &emitter, &cancel);
    let change = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        agent.request_mode(t8ful_core::Mode::Plan);
    };

    let (outcome, ()) = tokio::join!(run, change);

    assert_eq!(
        outcome.expect("turn succeeds").finish,
        t8ful_core::TurnFinish::ModeChanged
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        conversation
            .messages()
            .iter()
            .flat_map(t8ful_core::Message::results)
            .nth(1)
            .expect("second result")
            .status,
        ToolStatus::NotExecuted
    );
}

#[tokio::test]
async fn a_switch_is_announced_once_in_the_next_request_only() {
    let provider = ScriptedProvider::new(vec![
        ScriptedProvider::text_step("first"),
        ScriptedProvider::text_step("second"),
    ]);
    let requests = provider.requests();
    let mut agent = agent_with(provider, Vec::new(), t8ful_core::Mode::Coding);

    agent.switch_mode(t8ful_core::Mode::Plan);

    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("first turn");
    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("second turn");

    let requests = requests.lock().expect("request log");
    let first = &requests[0].system;
    let second = &requests[1].system;

    assert!(first.contains("<mode_change>"), "{first}");
    assert!(first.contains("from coding mode to plan mode"), "{first}");
    assert!(first.contains("do not modify project files now"), "{first}");
    assert!(!first.contains("from plan mode to coding mode"), "{first}");
    assert!(!second.contains("<mode_change>"), "{second}");
    assert!(second.contains("you are a test agent"), "{second}");
}

#[tokio::test]
async fn startup_resume_and_the_opposite_switch_carry_the_right_notices() {
    let provider = ScriptedProvider::new(vec![
        ScriptedProvider::text_step("startup"),
        ScriptedProvider::text_step("resume"),
        ScriptedProvider::text_step("switch"),
    ]);
    let requests = provider.requests();
    let mut agent = agent_with(provider, Vec::new(), t8ful_core::Mode::Coding);
    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();

    // Startup: the agent is built in a mode, so nothing is announced.
    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("startup turn");

    // Resume: syncing to the journal's mode is not a user switch.
    agent.set_mode(t8ful_core::Mode::Plan);
    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("resumed turn");

    // A real switch back to coding names both modes and its consequence.
    agent.switch_mode(t8ful_core::Mode::Coding);
    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("switch turn");

    let requests = requests.lock().expect("request log");

    assert!(!requests[0].system.contains("<mode_change>"));
    assert!(!requests[1].system.contains("<mode_change>"));

    let switched = &requests[2].system;

    assert!(
        switched.contains("from plan mode to coding mode"),
        "{switched}"
    );
    assert!(
        switched.contains("restrictions no longer apply"),
        "{switched}"
    );
    assert!(
        switched.contains("user's message is the instruction"),
        "{switched}"
    );
}

#[tokio::test]
async fn switching_to_the_current_mode_stores_no_notice() {
    let provider = ScriptedProvider::new(vec![ScriptedProvider::text_step("answer")]);
    let requests = provider.requests();
    let mut agent = agent_with(provider, Vec::new(), t8ful_core::Mode::Plan);

    agent.switch_mode(t8ful_core::Mode::Plan);

    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("turn");

    let requests = requests.lock().expect("request log");

    assert!(!requests[0].system.contains("<mode_change>"));
}

#[tokio::test]
async fn separate_compactions_use_distinct_request_prefixes() {
    let prefixes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider::new(vec![
        ScriptedProvider::text_step("first"),
        ScriptedProvider::text_step("second"),
    ]);
    let mut config = AgentConfig::new("test/model", "you are a test agent");

    config.context_limit = Some(TEST_CONTEXT_LIMIT);
    config.max_output_tokens = Some(20);
    config.request_namespace = "session-1".to_owned();

    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        Policy::new(Mode::Coding),
        config,
        ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
    );

    agent.set_compactor(Arc::new(RecordingCompactor {
        prefixes: Arc::clone(&prefixes),
    }));

    let mut conversation = compactable_conversation(10);

    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("first turn");

    for index in 10..13 {
        push_large_turn(&mut conversation, index);
    }

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("second turn");

    let prefixes = prefixes.lock().expect("prefix log");

    assert_eq!(prefixes.len(), 2, "{prefixes:?}");
    assert_ne!(prefixes[0], prefixes[1], "{prefixes:?}");
}

#[tokio::test]
async fn compaction_commits_a_projection_only_after_success() {
    let called = Arc::new(AtomicUsize::new(0));
    let prefix_len = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider::new(vec![ScriptedProvider::text_step("finish")]);
    let mut agent = agent_with(provider, Vec::new(), t8ful_core::Mode::Coding);
    let mut config = agent.config().clone();

    config.context_limit = Some(TEST_CONTEXT_LIMIT);
    config.max_output_tokens = Some(20);
    agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_step(
            "finish",
        )])),
        Arc::new(ToolRegistry::new()),
        Policy::new(Mode::Coding),
        config,
        ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
    );
    agent.set_compactor(Arc::new(FixedCompactor {
        outcome: Ok(t8ful_core::CompactionOutput {
            summary: "summary of the first turn".to_owned(),
            chunks: 1,
            usages: Vec::new(),
            attempts: Vec::new(),
        }),
        called: Arc::clone(&called),
        prefix_len: Arc::clone(&prefix_len),
    }));

    let mut conversation = compactable_conversation(10);

    let (emitter, mut _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let outcome = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("turn succeeds");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert!(prefix_len.load(Ordering::SeqCst) >= 2);
    assert!(agent.projection().is_some());

    let mut seen = Vec::new();

    while let Ok(event) = _events.try_recv() {
        seen.push(event.event);
    }

    assert!(seen.iter().any(|event| matches!(
        event,
        AgentEvent::CompactionStarted {
            estimated_tokens,
            context_limit,
        } if *context_limit == TEST_CONTEXT_LIMIT && *estimated_tokens > 0
            && *estimated_tokens < TEST_CONTEXT_LIMIT
    )));
}

#[tokio::test]
async fn compaction_failure_retains_the_original_context_and_stops() {
    let called = Arc::new(AtomicUsize::new(0));
    let agent = agent_with(
        ScriptedProvider::new(vec![]),
        Vec::new(),
        t8ful_core::Mode::Coding,
    );
    let mut config = agent.config().clone();

    config.context_limit = Some(TEST_CONTEXT_LIMIT);
    config.max_output_tokens = Some(20);
    let agent = {
        let mut rebuilt = Agent::new(
            Arc::new(ScriptedProvider::new(vec![])),
            Arc::new(ToolRegistry::new()),
            Policy::new(Mode::Coding),
            config,
            ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
        );
        rebuilt.set_compactor(Arc::new(FixedCompactor {
            outcome: Err(t8ful_core::CompactionError::Failed(
                "summary provider failed".to_owned(),
            )),
            called: Arc::clone(&called),
            prefix_len: Arc::new(AtomicUsize::new(0)),
        }));
        rebuilt
    };
    let mut conversation = compactable_conversation(10);

    let before = conversation.clone();
    let (emitter, mut events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let error = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect_err("compaction failure");

    assert!(matches!(error, AgentError::Compaction(_)), "{error}");
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert_eq!(conversation, before);
    assert!(agent.projection().is_none());

    let mut seen = Vec::new();

    while let Ok(event) = events.try_recv() {
        seen.push(event.event);
    }

    assert!(
        seen.iter().any(|event| matches!(
            event,
            AgentEvent::Error { message } if message.contains("summary provider failed")
        )),
        "{seen:?}"
    );
    assert!(
        seen.contains(&AgentEvent::TurnFinished {
            turn: 1,
            reason: "error".to_owned()
        }),
        "{seen:?}"
    );
}

#[tokio::test]
async fn provider_usage_is_the_compaction_baseline() {
    let called = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider::new(vec![
        vec![
            Ok(ProviderEvent::Usage(t8ful_core::Usage {
                input_tokens: Some(30_000),
                output_tokens: Some(1_000),
                ..t8ful_core::Usage::default()
            })),
            Ok(ProviderEvent::TextDelta("first".to_owned())),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ],
        ScriptedProvider::text_step("second"),
    ]);
    let mut config = AgentConfig::new("test/model", "you are a test agent");

    config.context_limit = Some(TEST_CONTEXT_LIMIT);

    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        Policy::new(Mode::Coding),
        config,
        ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
    );
    agent.set_compactor(Arc::new(FixedCompactor {
        outcome: Ok(t8ful_core::CompactionOutput {
            summary: "usage summary".to_owned(),
            chunks: 1,
            usages: Vec::new(),
            attempts: Vec::new(),
        }),
        called: Arc::clone(&called),
        prefix_len: Arc::new(AtomicUsize::new(0)),
    }));

    let mut conversation = compactable_conversation(9);
    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("first turn");

    assert_eq!(called.load(Ordering::SeqCst), 0);

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("second turn");

    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert!(agent.projection().is_some());
}

#[tokio::test]
async fn a_split_turn_is_summarized_as_two_ordered_segments() {
    let prefixes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent = {
        let mut agent = agent_with(
            ScriptedProvider::new(vec![ScriptedProvider::text_step("finish")]),
            Vec::new(),
            t8ful_core::Mode::Coding,
        );
        let mut config = agent.config().clone();

        config.context_limit = Some(TEST_CONTEXT_LIMIT);
        agent = Agent::new(
            Arc::new(ScriptedProvider::new(vec![
                ScriptedProvider::text_step("finish"),
                ScriptedProvider::text_step("again"),
            ])),
            Arc::new(ToolRegistry::new()),
            Policy::new(Mode::Coding),
            config,
            ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
        );
        agent.set_compactor(Arc::new(RecordingCompactor {
            prefixes: Arc::clone(&prefixes),
        }));
        agent
    };
    let mut conversation = split_turn_conversation();
    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("turn succeeds");

    let projection = agent.projection().expect("projection");
    let seen = prefixes.lock().expect("prefix log").clone();

    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(seen[0].ends_with("-history"), "{seen:?}");
    assert!(seen[1].ends_with("-turn"), "{seen:?}");
    assert_eq!(projection.summary, "summary\n\nsummary");
    assert!(projection.covered_messages > 0);

    let calls = seen.len();

    let (emitter, mut events) = EventEmitter::channel();
    let outcome = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("the turn continues when no newer prefix can be compacted");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);
    assert_eq!(prefixes.lock().expect("prefix log").len(), calls);
    assert!(
        std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
            event.event,
            AgentEvent::Warning { message } if message.contains("no safe compactable prefix")
        ))
    );
}

#[tokio::test]
async fn compaction_failure_keeps_an_existing_projection_and_continues() {
    let called = Arc::new(AtomicUsize::new(0));
    let mut agent = {
        let mut agent = agent_with(
            ScriptedProvider::new(vec![
                ScriptedProvider::text_step("first"),
                ScriptedProvider::text_step("second"),
            ]),
            Vec::new(),
            t8ful_core::Mode::Coding,
        );
        let mut config = agent.config().clone();

        config.context_limit = Some(TEST_CONTEXT_LIMIT);
        agent = Agent::new(
            Arc::new(ScriptedProvider::new(vec![
                ScriptedProvider::text_step("first"),
                ScriptedProvider::text_step("second"),
            ])),
            Arc::new(ToolRegistry::new()),
            Policy::new(Mode::Coding),
            config,
            ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
        );
        agent.set_compactor(Arc::new(FixedCompactor {
            outcome: Ok(t8ful_core::CompactionOutput {
                summary: "existing summary".to_owned(),
                chunks: 1,
                usages: Vec::new(),
                attempts: Vec::new(),
            }),
            called: Arc::clone(&called),
            prefix_len: Arc::new(AtomicUsize::new(0)),
        }));
        agent
    };
    let mut conversation = compactable_conversation(10);
    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();

    agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("first turn");

    let covered = agent.projection().expect("projection").covered_messages;

    for index in 10..13 {
        push_large_turn(&mut conversation, index);
    }

    agent.set_compactor(Arc::new(FixedCompactor {
        outcome: Err(t8ful_core::CompactionError::Failed(
            "summary provider failed".to_owned(),
        )),
        called: Arc::clone(&called),
        prefix_len: Arc::new(AtomicUsize::new(0)),
    }));

    let (emitter, mut events) = EventEmitter::channel();
    let outcome = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("turn continues with the existing projection");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);
    assert_eq!(
        agent.projection().expect("projection").covered_messages,
        covered
    );
    assert!(
        std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
            event.event,
            AgentEvent::Warning { message } if message.contains("summary provider failed")
        ))
    );
}

struct RecordingCompactor {
    prefixes: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl t8ful_core::ContextCompactor for RecordingCompactor {
    async fn summarize(
        &self,
        _prefix: &[t8ful_core::Message],
        _model: &str,
        _agent: &str,
        request_prefix: &str,
        _limits: t8ful_core::compaction::CompactionLimits,
        _cancel: &CancellationToken,
    ) -> Result<t8ful_core::CompactionOutput, t8ful_core::CompactionError> {
        self.prefixes
            .lock()
            .expect("prefix log")
            .push(request_prefix.to_owned());

        Ok(t8ful_core::CompactionOutput {
            summary: "summary".to_owned(),
            chunks: 1,
            usages: Vec::new(),
            attempts: Vec::new(),
        })
    }
}

struct FailingJournal {
    fail_dispatch: bool,
    fail_result: bool,
    dispatches: Arc<AtomicUsize>,
    results: Arc<AtomicUsize>,
}

#[async_trait]
impl t8ful_core::AgentJournal for FailingJournal {
    async fn record_message(
        &self,
        _message: &t8ful_core::Message,
    ) -> Result<(), t8ful_core::JournalError> {
        Ok(())
    }

    async fn record_model_attempt(
        &self,
        _attempt: &t8ful_core::ModelAttempt,
    ) -> Result<(), t8ful_core::JournalError> {
        Ok(())
    }

    async fn record_dispatch(&self, _call: &ToolCall) -> Result<(), t8ful_core::JournalError> {
        self.dispatch_calls();

        if self.fail_dispatch {
            return Err(t8ful_core::JournalError::new("disk full before dispatch"));
        }

        Ok(())
    }

    async fn record_tool_result(
        &self,
        _result: &t8ful_core::ToolResult,
    ) -> Result<(), t8ful_core::JournalError> {
        self.result_calls();

        if self.fail_result {
            return Err(t8ful_core::JournalError::new("disk full after execution"));
        }

        Ok(())
    }

    async fn record_usage(
        &self,
        _usage: &t8ful_core::UsageRecord,
    ) -> Result<(), t8ful_core::JournalError> {
        Ok(())
    }

    async fn record_compaction(
        &self,
        _projection: &t8ful_core::CompactionRecord,
    ) -> Result<(), t8ful_core::JournalError> {
        Ok(())
    }
}

impl FailingJournal {
    fn dispatch_calls(&self) {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
    }

    fn result_calls(&self) {
        self.results.fetch_add(1, Ordering::SeqCst);
    }
}

fn journalled_agent(journal: Arc<FailingJournal>, calls: Arc<AtomicUsize>) -> Agent {
    let mut registry = ToolRegistry::new();

    registry.register(CountingTool::new(
        "write",
        ToolEffect::Mutating,
        Arc::clone(&calls),
    ));

    let mut agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![call("call-1", "write")]),
            ScriptedProvider::text_step("done"),
        ])),
        Arc::new(registry),
        Policy::new(Mode::Coding),
        AgentConfig::new("test/model", "test"),
        ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
    );

    agent.set_journal(journal);

    agent
}

#[tokio::test]
async fn a_journal_failure_before_dispatch_never_executes_the_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let journal = Arc::new(FailingJournal {
        fail_dispatch: true,
        fail_result: false,
        dispatches: Arc::new(AtomicUsize::new(0)),
        results: Arc::new(AtomicUsize::new(0)),
    });
    let agent = journalled_agent(journal, Arc::clone(&calls));
    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();
    let error = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect_err("journal failure");

    assert!(matches!(error, AgentError::Journal(_)), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        conversation.unanswered_tool_calls().is_empty(),
        "a dispatch-journal failure left a call unanswered: {:?}",
        conversation.unanswered_tool_calls()
    );
}

#[tokio::test]
async fn a_result_journal_failure_keeps_the_dispatch_intent_for_resume() {
    let calls = Arc::new(AtomicUsize::new(0));
    let journal = Arc::new(FailingJournal {
        fail_dispatch: false,
        fail_result: true,
        dispatches: Arc::new(AtomicUsize::new(0)),
        results: Arc::new(AtomicUsize::new(0)),
    });
    let agent = journalled_agent(Arc::clone(&journal), Arc::clone(&calls));
    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();
    let error = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect_err("journal failure");

    assert!(matches!(error, AgentError::Journal(_)), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(journal.dispatches.load(Ordering::SeqCst), 1);

    // The assistant message carrying the call is already in the conversation, so
    // the turn must still leave a result for it: a provider rejects the next
    // request when a tool call has no result.
    assert!(
        conversation.unanswered_tool_calls().is_empty(),
        "a result-journal failure left a call unanswered: {:?}",
        conversation.unanswered_tool_calls()
    );
}

/// A journal failure on the first result must not strand the rest of the batch.
#[tokio::test]
async fn a_result_journal_failure_answers_every_call_it_never_reached() {
    let calls = Arc::new(AtomicUsize::new(0));
    let journal = Arc::new(FailingJournal {
        fail_dispatch: false,
        fail_result: true,
        dispatches: Arc::new(AtomicUsize::new(0)),
        results: Arc::new(AtomicUsize::new(0)),
    });

    let mut registry = ToolRegistry::new();

    registry.register(CountingTool::new(
        "write",
        ToolEffect::Mutating,
        Arc::clone(&calls),
    ));

    let mut agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_step(vec![call("call-1", "write"), call("call-2", "write")]),
            ScriptedProvider::text_step("done"),
        ])),
        Arc::new(registry),
        Policy::new(Mode::Coding),
        AgentConfig::new("test/model", "test"),
        ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
    );

    agent.set_journal(journal);

    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let mut conversation = Conversation::new();
    let error = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect_err("journal failure");

    assert!(matches!(error, AgentError::Journal(_)), "{error}");
    // Only the first call ran; the journal refused to record its result, so the
    // second call must never start and must still be answered.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        conversation.unanswered_tool_calls().is_empty(),
        "the batch left calls unanswered: {:?}",
        conversation.unanswered_tool_calls()
    );
}

#[tokio::test]
async fn cancelled_compaction_retains_the_original_context() {
    let called = Arc::new(AtomicUsize::new(0));
    let mut agent = agent_with(
        ScriptedProvider::new(vec![]),
        Vec::new(),
        t8ful_core::Mode::Coding,
    );
    let mut config = agent.config().clone();

    config.context_limit = Some(TEST_CONTEXT_LIMIT);
    config.max_output_tokens = Some(20);
    agent = {
        let mut rebuilt = Agent::new(
            Arc::new(ScriptedProvider::new(vec![])),
            Arc::new(ToolRegistry::new()),
            Policy::new(Mode::Coding),
            config,
            ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
        );
        rebuilt.set_compactor(Arc::new(FixedCompactor {
            outcome: Err(t8ful_core::CompactionError::Cancelled),
            called: Arc::clone(&called),
            prefix_len: Arc::new(AtomicUsize::new(0)),
        }));
        rebuilt
    };
    let mut conversation = compactable_conversation(10);

    let before = conversation.clone();
    let (emitter, _events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let error = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect_err("cancelled compaction");

    assert!(matches!(error, AgentError::Compaction(_)), "{error}");
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert_eq!(conversation, before);
    assert!(agent.projection().is_none());
}

/// A window with nothing safe to compact must still run its turn.
///
/// "No safe compactable prefix" is not a compaction failure: with a small
/// window the static system and tool estimate alone can reach the trigger before
/// the first request, and ending the turn there would make the fallback window
/// unusable rather than merely uncompacted.
#[tokio::test]
async fn a_window_with_nothing_to_compact_still_runs_the_turn() {
    let mut config = AgentConfig::new("test/model", "you are a test agent");

    config.context_limit = Some(500);
    config.max_output_tokens = Some(50);

    let mut agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_step(
            "answer",
        )])),
        Arc::new(ToolRegistry::new()),
        Policy::new(Mode::Coding),
        config,
        ToolContext::new("/tmp/project".into(), "/tmp/project".into()),
    );

    agent.set_compactor(Arc::new(FixedCompactor {
        outcome: Ok(t8ful_core::CompactionOutput {
            summary: "unused".to_owned(),
            chunks: 1,
            usages: Vec::new(),
            attempts: Vec::new(),
        }),
        called: Arc::new(AtomicUsize::new(0)),
        prefix_len: Arc::new(AtomicUsize::new(0)),
    }));

    let mut conversation = Conversation::new();

    // One message: it fills the trigger, and a single message can never be cut
    // into a summarized prefix plus a retained suffix.
    conversation.push_user_text("hello ".repeat(400));

    let (emitter, mut events) = EventEmitter::channel();
    let cancel = CancellationToken::new();
    let outcome = agent
        .run(&mut conversation, &emitter, &cancel)
        .await
        .expect("a window with nothing to compact must not fail the turn");

    assert_eq!(outcome.finish, t8ful_core::TurnFinish::Stop);

    let warned = std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
        matches!(
            event.event,
            AgentEvent::Warning { message } if message.contains("no safe compactable prefix")
        )
    });

    assert!(
        warned,
        "the turn continued silently instead of reporting it"
    );
}
