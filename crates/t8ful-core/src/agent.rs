use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use futures_util::StreamExt;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::compaction::{CompactionError, CompactionOutput, ContextCompactor};
use crate::content::Content;
use crate::context::{
    ContextBudget, Projection, estimate_messages, estimate_projected, estimate_tokens,
};
use crate::error::{AgentError, ProviderError};
use crate::event::{AgentEvent, EventEmitter};
use crate::journal::{AgentJournal, CompactionRecord, ModelAttempt, NullJournal, UsageRecord};
use crate::message::{Conversation, Message};
use crate::notice::Notice;
use crate::policy::{Decision, Mode, Policy};
use crate::provider::{
    FinishReason, Provider, ProviderEvent, ProviderRequest, ReasoningEffort, ReasoningProfile,
};
use crate::tool::{ToolCall, ToolContext, ToolError, ToolRegistry, ToolResult, ToolStatus};

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub system: String,
    /// Prompt variants used when the user switches modes at a turn boundary.
    pub system_coding: Option<String>,
    pub system_plan: Option<String>,
    /// Per-turn model step limit. `None` means a turn is unbounded and only
    /// ends through a finish reason, input, a mode change, cancellation, or an
    /// error.
    pub max_steps: Option<u32>,
    pub max_output_tokens: Option<u32>,
    /// Effective context window, when known or configured.
    pub context_limit: Option<u64>,
    /// `Some(false)` means the selected model is known not to accept images.
    pub supports_images: Option<bool>,
    /// `Some(false)` means the selected model does not advertise reasoning.
    pub supports_reasoning: Option<bool>,
    pub agent_id: String,
    /// Unique per run; keeps request IDs distinct across resumed sessions.
    pub request_namespace: String,
}

impl AgentConfig {
    pub fn new(model: impl Into<String>, system: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: system.into(),
            system_coding: None,
            system_plan: None,
            max_steps: None,
            max_output_tokens: None,
            context_limit: None,
            supports_images: None,
            supports_reasoning: None,
            agent_id: "root".to_owned(),
            request_namespace: "run".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFinish {
    Stop,
    Length,
    MaxSteps,
    InputRequired,
    PlanPresented,
    ModeChanged,
}

impl TurnFinish {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::MaxSteps => "max_steps",
            Self::InputRequired => "input_required",
            Self::PlanPresented => "plan_presented",
            Self::ModeChanged => "mode_changed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnOutcome {
    pub steps: u32,
    pub finish: TurnFinish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeRequest {
    None,
    Coding,
    Plan,
}

impl ModeRequest {
    fn from_raw(value: u8) -> Self {
        match value {
            1 => Self::Coding,
            2 => Self::Plan,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct UsageBaseline {
    /// Number of raw conversation messages covered by the measured request.
    message_index: usize,
    tokens: u64,
}

/// Drives one conversation turn: request, stream, gate, dispatch, repeat.
///
/// The loop never retries a tool call. A call that already has a recorded result
/// is refused, and a call interrupted mid-flight is reported as indeterminate,
/// so a crash, cancellation, or resumed turn cannot repeat a side effect.
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    policy: Policy,
    config: AgentConfig,
    context: ToolContext,
    turns: AtomicU64,
    turn_base: AtomicU64,
    compactions: AtomicU64,
    journal: Arc<dyn AgentJournal>,
    compactor: Option<Arc<dyn ContextCompactor>>,
    projection: Mutex<Option<Projection>>,
    /// Provider-reported request size for the newest assistant message.
    usage_baseline: Mutex<Option<UsageBaseline>>,
    steering: Mutex<VecDeque<String>>,
    steering_notify: Notify,
    requested_mode: AtomicU8,
    mode_notify: Notify,
    /// A real switch waiting to be announced in the next request built.
    mode_change: Mutex<Option<(Mode, Mode)>>,
    warned_unknown_images: AtomicBool,
    reasoning_effort: Mutex<ReasoningEffort>,
    /// The levels the selected model offers and their provider values.
    reasoning_profile: ReasoningProfile,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        policy: Policy,
        config: AgentConfig,
        context: ToolContext,
    ) -> Self {
        Self {
            provider,
            tools,
            policy,
            config,
            context,
            turns: AtomicU64::new(0),
            turn_base: AtomicU64::new(0),
            compactions: AtomicU64::new(0),
            journal: Arc::new(NullJournal),
            compactor: None,
            projection: Mutex::new(None),
            usage_baseline: Mutex::new(None),
            steering: Mutex::new(VecDeque::new()),
            steering_notify: Notify::new(),
            requested_mode: AtomicU8::new(0),
            mode_notify: Notify::new(),
            mode_change: Mutex::new(None),
            warned_unknown_images: AtomicBool::new(false),
            reasoning_effort: Mutex::new(ReasoningEffort::Default),
            reasoning_profile: ReasoningProfile::unknown(),
        }
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// The tool names the live registry offers, in sorted order.
    ///
    /// Exposed for tests and diagnostics that have to prove a configured tool
    /// was withheld rather than merely unused.
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.names()
    }

    /// Replace the tool registry at a turn boundary.
    ///
    /// The driver calls this while no turn is running, before the first
    /// request, when discovery settled after the session was built. Replacing
    /// mid-turn would change the advertised tool list under a request that is
    /// already in flight.
    pub fn set_tools(&mut self, tools: Arc<ToolRegistry>) {
        self.tools = tools;
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// The tool context this agent dispatches with.
    ///
    /// Exposed so a caller can verify what a child inherited — the sandbox, the
    /// limits, and any environment additions — without rebuilding it.
    pub fn tool_context(&self) -> &ToolContext {
        &self.context
    }

    pub fn set_journal(&mut self, journal: Arc<dyn AgentJournal>) {
        self.journal = journal;
    }

    /// Replace the provider used for the next request.
    ///
    /// The agent keeps no provider state beyond the handle, so a credential
    /// change never needs a rebuild of the running agent.
    pub fn set_provider(&mut self, provider: Arc<dyn Provider>) {
        self.provider = provider;
    }

    pub fn set_compactor(&mut self, compactor: Arc<dyn ContextCompactor>) {
        self.compactor = Some(compactor);
    }

    pub fn set_projection(&self, projection: Option<Projection>) {
        *self
            .projection
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = projection;
    }

    pub fn projection(&self) -> Option<Projection> {
        self.projection
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn set_turn_base(&self, turns: u64) {
        self.turn_base.store(turns, Ordering::Relaxed);
    }

    /// Apply the model's context and capability limits together with its name.
    /// Context and output limits must never be left over from a previous model.
    ///
    /// A model that does not advertise reasoning drops a non-default effort
    /// back to `default`; the returned message explains the reset so the
    /// frontend can surface it.
    pub fn set_model_profile(
        &mut self,
        model: impl Into<String>,
        context_limit: u64,
        max_output_tokens: u32,
        supports_images: Option<bool>,
        supports_reasoning: Option<bool>,
    ) -> Option<String> {
        self.config.model = model.into();
        self.config.context_limit = Some(context_limit);
        self.config.max_output_tokens = Some(max_output_tokens);
        self.config.supports_images = supports_images;
        self.config.supports_reasoning = supports_reasoning;
        self.warned_unknown_images.store(false, Ordering::Relaxed);
        self.clear_usage_baseline();

        if supports_reasoning == Some(false) && self.reasoning_effort() != ReasoningEffort::Default
        {
            self.set_reasoning_effort(ReasoningEffort::Default);

            return Some(format!(
                "model `{}` does not advertise reasoning; reasoning effort was reset to default",
                self.config.model
            ));
        }

        None
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn supports_reasoning(&self) -> Option<bool> {
        self.config.supports_reasoning
    }

    pub fn reasoning_effort(&self) -> ReasoningEffort {
        *self
            .reasoning_effort
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Install the selected model's reasoning levels.
    ///
    /// A current effort the new model does not offer is clamped, so switching
    /// models never leaves an unsupported level selected. The returned message
    /// reports that clamp so the frontend can surface it.
    pub fn set_reasoning_profile(&mut self, profile: ReasoningProfile) -> Option<String> {
        self.reasoning_profile = profile;

        let requested = self.reasoning_effort();
        let effective = self.reasoning_profile.clamp(requested);

        if effective == requested {
            return None;
        }

        self.set_reasoning_effort(effective);

        Some(format!(
            "model `{}` does not offer reasoning level `{}`; using `{}`",
            self.config.model,
            requested.label(),
            effective.label()
        ))
    }

    pub fn reasoning_profile(&self) -> &ReasoningProfile {
        &self.reasoning_profile
    }

    pub fn set_reasoning_effort(&self, effort: ReasoningEffort) {
        *self
            .reasoning_effort
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = effort;
    }

    /// Apply the user's mode only at a turn boundary.
    ///
    /// This is the sync path used at startup and after resume: it changes the
    /// gate and the prompt but does not announce the change, because the
    /// conversation has no earlier mode to correct.
    pub fn set_mode(&mut self, mode: Mode) {
        self.policy.set_mode(mode);
        self.context.mode = mode;

        let system = match mode {
            Mode::Coding => self.config.system_coding.as_ref(),
            Mode::Plan => self.config.system_plan.as_ref(),
        };

        if let Some(system) = system {
            self.config.system.clone_from(system);
        }

        self.requested_mode.store(0, Ordering::SeqCst);
    }

    /// Replace both mode prompts at a turn boundary.
    ///
    /// This is the same derivation [`Agent::set_mode`] makes, so the active
    /// prompt always matches the mode the gate reports. It is used when a
    /// deferred discovery step rebuilds the prompts from a wider tool list.
    pub fn set_system_prompts(&mut self, coding: String, plan: String) {
        self.config.system_coding = Some(coding);
        self.config.system_plan = Some(plan);

        let system = match self.policy.mode() {
            Mode::Coding => self.config.system_coding.as_ref(),
            Mode::Plan => self.config.system_plan.as_ref(),
        };

        if let Some(system) = system {
            self.config.system.clone_from(system);
        }
    }

    /// Apply a user-initiated switch and announce it in the next request.
    ///
    /// The notice names both modes so the model never has to read the live mode
    /// from an older message. It is consumed when the next request is built,
    /// even if that request fails; the mode section itself stays authoritative.
    pub fn switch_mode(&mut self, mode: Mode) {
        let previous = self.policy.mode();

        self.set_mode(mode);

        if previous == mode {
            return;
        }

        *self
            .mode_change
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some((previous, mode));
    }

    fn take_mode_change(&self) -> Option<(Mode, Mode)> {
        self.mode_change
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }

    /// The system prompt for the next request, with a pending switch notice.
    fn request_system(&self) -> String {
        let mut system = self.config.system.clone();

        if let Some((previous, mode)) = self.take_mode_change()
            && let Some(section) = crate::policy::mode_change_section(previous, mode)
        {
            system.push_str("\n\n");
            system.push_str(&section);
        }

        system
    }

    /// Ask the running turn to stop dispatching at the next safe boundary.
    pub fn request_mode(&self, mode: Mode) {
        self.requested_mode.store(
            match mode {
                Mode::Coding => 1,
                Mode::Plan => 2,
            },
            Ordering::SeqCst,
        );
        self.mode_notify.notify_waiters();
    }

    pub fn has_pending_mode_request(&self) -> bool {
        self.requested_mode() != ModeRequest::None
    }

    pub fn steer(&self, text: impl Into<String>) {
        let text = text.into();

        if text.trim().is_empty() {
            return;
        }

        self.steering
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_back(text);
        self.steering_notify.notify_waiters();
    }

    fn has_steering(&self) -> bool {
        !self
            .steering
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_empty()
    }

    fn take_steering(&self) -> Vec<String> {
        self.steering
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .drain(..)
            .collect()
    }

    /// Drain steering that arrived after the turn reached its final boundary.
    /// The driver turns these into follow-up prompts rather than losing them.
    pub fn take_pending_steering(&self) -> Vec<String> {
        self.take_steering()
    }

    /// Inject a completion notice from outside the loop.
    ///
    /// This is the single injection path: the driver uses it for an idle wake
    /// and for the notices that precede the next submitted prompt. The notice
    /// becomes a user-role message and is journaled exactly once; it never
    /// touches steering, so no in-flight call is disturbed.
    pub async fn deliver_notice(
        &self,
        notice: Notice,
        conversation: &mut Conversation,
        events: &EventEmitter,
    ) -> Result<(), AgentError> {
        let text = notice.render();
        let message = Message::user_text(text);

        self.journal
            .record_message(&message)
            .await
            .map_err(|error| AgentError::Journal(error.0))?;
        conversation.push(message);

        match notice {
            Notice::Agent(notice) => {
                events.send(AgentEvent::AgentNoticeDelivered {
                    agent: notice.report.agent.to_string(),
                });
            }
            Notice::Process(notice) => {
                events.send(AgentEvent::ProcessNoticeDelivered {
                    process_id: notice.process_id,
                });
            }
        }

        Ok(())
    }

    fn requested_mode(&self) -> ModeRequest {
        ModeRequest::from_raw(self.requested_mode.load(Ordering::SeqCst))
    }

    pub async fn run(
        &self,
        conversation: &mut Conversation,
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<TurnOutcome, AgentError> {
        let turn =
            self.turn_base.load(Ordering::Relaxed) + self.turns.fetch_add(1, Ordering::Relaxed) + 1;

        events.send(AgentEvent::TurnStarted { turn });

        let tool_specs = self.tools.specs();
        let mut step = 0;

        loop {
            if self
                .config
                .max_steps
                .is_some_and(|limit| step >= limit.max(1))
            {
                break;
            }

            if self.requested_mode() != ModeRequest::None {
                events.send(AgentEvent::TurnFinished {
                    turn,
                    reason: TurnFinish::ModeChanged.label().to_owned(),
                });

                return Ok(TurnOutcome {
                    steps: step,
                    finish: TurnFinish::ModeChanged,
                });
            }

            if self.has_steering() {
                self.deliver_steering(conversation, events).await?;
            }

            if self.should_compact(conversation, &tool_specs)
                && let Err(error) = self.compact(conversation, events, cancel).await
            {
                events.send(AgentEvent::Error {
                    message: error.to_string(),
                });
                events.send(AgentEvent::TurnFinished {
                    turn,
                    reason: "error".to_owned(),
                });

                return Err(error);
            }

            let messages = self.request_messages(conversation);

            if let Err(error) = self.check_image_support(&messages, events) {
                events.send(AgentEvent::Error {
                    message: error.to_string(),
                });
                events.send(AgentEvent::TurnFinished {
                    turn,
                    reason: "error".to_owned(),
                });

                return Err(error);
            }

            step += 1;

            let request_id = format!("{}-{}-{}", self.config.request_namespace, turn, step);
            let attempt = ModelAttempt {
                request_id: request_id.clone(),
                model: self.config.model.clone(),
                agent: self.config.agent_id.clone(),
                step,
                max_output_tokens: self.config.max_output_tokens,
                message_count: messages.len(),
            };

            self.journal
                .record_model_attempt(&attempt)
                .await
                .map_err(|error| AgentError::Journal(error.0))?;

            events.send(AgentEvent::ModelStarted {
                model: self.config.model.clone(),
                step,
            });

            let system = self.request_system();
            let reasoning = self.reasoning_profile.clamp(self.reasoning_effort());
            let mut request = ProviderRequest {
                model: self.config.model.clone(),
                system: system.clone(),
                messages,
                tools: tool_specs.clone(),
                max_output_tokens: self.config.max_output_tokens,
                reasoning,
                reasoning_value: self.reasoning_profile.provider_value(reasoning),
            };

            // The system prompt is sent separately by the provider. Remove the
            // projection marker from the visible message list on providers that
            // require a leading system prompt only.
            if let Some(projection) = self.projection()
                && let Some(first) = request.messages.first()
                && first.role == crate::message::Role::System
                && first.text().starts_with("[earlier conversation summary]")
            {
                request.system = format!(
                    "{system}\n\n[earlier conversation summary]\n{}",
                    projection.summary
                );
                request.messages.remove(0);
            }

            let mut stream = self.provider.stream(request);
            let mut text = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut finish: Option<FinishReason> = None;
            let mut usage_failed = false;
            let mut usage_seen = false;
            let mut step_tokens = None;

            loop {
                let next = tokio::select! {
                    () = cancel.cancelled() => {
                        events.send(AgentEvent::Error {
                            message: "turn cancelled while waiting for the model".to_owned(),
                        });
                        events.send(AgentEvent::TurnFinished {
                            turn,
                            reason: "cancelled".to_owned(),
                        });

                        return Err(AgentError::Cancelled);
                    }
                    next = stream.next() => next,
                };

                let Some(item) = next else {
                    break;
                };

                match item {
                    Err(error) => {
                        events.send(AgentEvent::Error {
                            message: error.to_string(),
                        });
                        events.send(AgentEvent::TurnFinished {
                            turn,
                            reason: "error".to_owned(),
                        });

                        return Err(error.into());
                    }
                    Ok(ProviderEvent::ReasoningDelta(delta)) => {
                        events.send(AgentEvent::ReasoningDelta { text: delta });
                    }
                    Ok(ProviderEvent::TextDelta(delta)) => {
                        events.send(AgentEvent::TextDelta {
                            text: delta.clone(),
                        });
                        text.push_str(&delta);
                    }
                    Ok(ProviderEvent::ToolCallComplete(call)) => calls.push(call),
                    Ok(ProviderEvent::Usage(usage)) => {
                        usage_seen = true;
                        step_tokens = usage.context_tokens();
                        let record = UsageRecord {
                            request_id: request_id.clone(),
                            model: self.config.model.clone(),
                            agent: self.config.agent_id.clone(),
                            kind: "turn".to_owned(),
                            usage,
                        };

                        if let Err(error) = self.journal.record_usage(&record).await {
                            usage_failed = true;

                            events.send(AgentEvent::Error {
                                message: error.to_string(),
                            });
                        } else {
                            events.send(usage_event(&record));
                        }
                    }
                    Ok(ProviderEvent::Finished(reason)) => finish = Some(reason),
                }
            }

            if usage_failed {
                return Err(AgentError::Journal(
                    "a usage record could not be persisted; the turn stopped".to_owned(),
                ));
            }

            if !usage_seen {
                let record = UsageRecord {
                    request_id: request_id.clone(),
                    model: self.config.model.clone(),
                    agent: self.config.agent_id.clone(),
                    kind: "turn".to_owned(),
                    usage: crate::usage::Usage::default(),
                };

                self.journal
                    .record_usage(&record)
                    .await
                    .map_err(|error| AgentError::Journal(error.0))?;
                events.send(usage_event(&record));
            }

            if finish.is_none() {
                let message = "the provider stream ended without a finish reason; the response is \
                               incomplete";

                events.send(AgentEvent::Error {
                    message: message.to_owned(),
                });
                events.send(AgentEvent::TurnFinished {
                    turn,
                    reason: "error".to_owned(),
                });

                return Err(AgentError::Provider(ProviderError::IncompleteStream(
                    message.to_owned(),
                )));
            }

            if !calls.is_empty()
                && let Some(message) = invalid_batch(&calls, finish.as_ref())
            {
                events.send(AgentEvent::Error {
                    message: message.to_owned(),
                });
                events.send(AgentEvent::TurnFinished {
                    turn,
                    reason: "error".to_owned(),
                });

                return Err(AgentError::Provider(ProviderError::MalformedStream(
                    message,
                )));
            }

            if !text.is_empty() || !calls.is_empty() {
                let mut content = Vec::new();

                if !text.is_empty() {
                    content.push(Content::text(text.clone()));
                }

                content.extend(calls.iter().cloned().map(Content::ToolCall));
                let message = Message::assistant(content);

                self.journal
                    .record_message(&message)
                    .await
                    .map_err(|error| AgentError::Journal(error.0))?;
                conversation.push(message);

                if !text.is_empty() {
                    events.send(AgentEvent::TextCompleted { text: text.clone() });
                }
            }

            if let Some(tokens) = step_tokens {
                self.set_usage_baseline(conversation.len(), tokens);
            }

            if calls.is_empty() {
                if let Some(tag) = fabricated_markup(&text) {
                    events.send(AgentEvent::Warning {
                        message: format!(
                            "the model wrote a `<{tag}>` block as text instead of calling a tool; \
                             nothing was dispatched"
                        ),
                    });
                }

                if self.has_steering() {
                    self.deliver_steering(conversation, events).await?;

                    continue;
                }

                if self.requested_mode() != ModeRequest::None {
                    events.send(AgentEvent::TurnFinished {
                        turn,
                        reason: TurnFinish::ModeChanged.label().to_owned(),
                    });

                    return Ok(TurnOutcome {
                        steps: step,
                        finish: TurnFinish::ModeChanged,
                    });
                }

                let finish = match finish {
                    Some(FinishReason::Length) => TurnFinish::Length,
                    _ => TurnFinish::Stop,
                };

                events.send(AgentEvent::TurnFinished {
                    turn,
                    reason: finish.label().to_owned(),
                });

                return Ok(TurnOutcome {
                    steps: step,
                    finish,
                });
            }

            let mut results: Vec<ToolResult> = Vec::with_capacity(calls.len());
            let mut input_required = false;
            let mut plan_presented = false;
            let mut cancelled = false;
            let mut mode_interrupted = false;
            let mut journal_error: Option<AgentError> = None;

            for (index, call) in calls.iter().enumerate() {
                if self.requested_mode() != ModeRequest::None {
                    journal_error = self
                        .mark_not_executed(
                            &calls[index..],
                            "not executed: the user changed the mode before this call started",
                            &mut results,
                            events,
                        )
                        .await;
                    mode_interrupted = true;

                    break;
                }

                events.send(AgentEvent::ToolCallStarted {
                    call_id: call.id.as_str().to_owned(),
                    tool: call.name.clone(),
                    arguments: call.arguments.clone(),
                });

                if conversation.has_result_for(call.id.as_str()) {
                    let result = ToolResult::new(
                        call,
                        ToolStatus::Error,
                        format!(
                            "call {} already has a recorded result; refusing to run it twice",
                            call.id
                        ),
                        false,
                    );

                    events.send(tool_result_event(&result, None));
                    results.push(result);

                    continue;
                }

                let Some(tool) = self.tools.get(&call.name) else {
                    let result = ToolResult::denied(call, format!("unknown tool `{}`", call.name));

                    if let Err(error) = self.journal.record_tool_result(&result).await {
                        results.push(result);
                        answer_unreached(&calls[index + 1..], &mut results, journal_failed());
                        journal_error = Some(AgentError::Journal(error.0));

                        break;
                    }

                    events.send(tool_result_event(&result, None));
                    results.push(result);

                    continue;
                };

                if let Decision::Deny { reason } = self.policy.evaluate(tool.effect()) {
                    // A tool whose generic plan-mode message would mislead gets
                    // to say why this particular call was refused.
                    let reason = tool.denial_reason().unwrap_or(reason);
                    let result = ToolResult::denied(call, reason);

                    if let Err(error) = self.journal.record_tool_result(&result).await {
                        results.push(result);
                        answer_unreached(&calls[index + 1..], &mut results, journal_failed());
                        journal_error = Some(AgentError::Journal(error.0));

                        break;
                    }

                    events.send(tool_result_event(&result, None));
                    results.push(result);

                    continue;
                }

                if let Err(error) = self.journal.record_dispatch(call).await {
                    // This call never started and neither did the rest of the
                    // batch, so answer all of them before the error ends the turn.
                    answer_unreached(&calls[index..], &mut results, journal_failed());
                    journal_error = Some(AgentError::Journal(error.0));

                    break;
                }

                let context = self.tool_context_for(call, events);
                let dispatched = tokio::select! {
                    biased;
                    () = cancel.cancelled() => None,
                    outcome = tool.execute(call, &context) => Some(outcome),
                };

                let mut process_id = None;
                let result = match dispatched {
                    None => {
                        cancelled = true;

                        ToolResult::new(
                            call,
                            ToolStatus::Indeterminate,
                            "cancelled while running; the outcome is unknown and the call will not be replayed",
                            false,
                        )
                    }
                    Some(Ok(output)) => {
                        process_id = output
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.get("process_id"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);

                        ToolResult::new(call, ToolStatus::Ok, output.content, output.truncated)
                    }
                    Some(Err(ToolError::InputRequired(message))) => {
                        input_required = true;

                        ToolResult::new(call, ToolStatus::InputRequired, message, false)
                    }
                    Some(Err(error)) => {
                        ToolResult::new(call, ToolStatus::Error, error.to_string(), false)
                    }
                };

                if let Err(error) = self.journal.record_tool_result(&result).await {
                    // The journal cannot say whether this call finished, so the
                    // in-memory batch keeps the result it knows and the calls it
                    // never reached are answered before the error ends the turn.
                    results.push(result);
                    answer_unreached(&calls[index + 1..], &mut results, journal_failed());
                    journal_error = Some(AgentError::Journal(error.0));

                    break;
                }

                events.send(tool_result_event(&result, process_id));

                if result.status == ToolStatus::Ok && call.name == "present_plan" {
                    plan_presented = true;
                }

                results.push(result);

                if cancelled {
                    journal_error = self
                        .mark_not_executed(
                            &calls[index + 1..],
                            "not executed: the turn was cancelled",
                            &mut results,
                            events,
                        )
                        .await;

                    break;
                }

                if self.requested_mode() != ModeRequest::None {
                    journal_error = self
                        .mark_not_executed(
                            &calls[index + 1..],
                            "not executed: the user changed the mode after the current call finished",
                            &mut results,
                            events,
                        )
                        .await;
                    mode_interrupted = true;

                    break;
                }
            }

            if !results.is_empty() {
                conversation.push(Message::tool_results(results));
            }

            // Reported only once the batch is closed: a provider rejects the
            // next request when a tool call has no result.
            if let Some(error) = journal_error {
                return Err(error);
            }

            if cancelled {
                events.send(AgentEvent::TurnFinished {
                    turn,
                    reason: "cancelled".to_owned(),
                });

                return Err(AgentError::Cancelled);
            }

            if plan_presented {
                if let Some(plan) = calls
                    .iter()
                    .find(|call| call.name == "present_plan")
                    .and_then(|call| call.arguments.get("plan"))
                    .and_then(serde_json::Value::as_str)
                {
                    events.send(AgentEvent::PlanPresented {
                        plan: plan.to_owned(),
                    });
                }

                events.send(AgentEvent::TurnFinished {
                    turn,
                    reason: TurnFinish::PlanPresented.label().to_owned(),
                });

                return Ok(TurnOutcome {
                    steps: step,
                    finish: TurnFinish::PlanPresented,
                });
            }

            if mode_interrupted {
                if self.has_steering() {
                    self.deliver_steering(conversation, events).await?;
                }

                events.send(AgentEvent::TurnFinished {
                    turn,
                    reason: TurnFinish::ModeChanged.label().to_owned(),
                });

                return Ok(TurnOutcome {
                    steps: step,
                    finish: TurnFinish::ModeChanged,
                });
            }

            // A steer never cancels a call. It is delivered only after every
            // call in this assistant message has finished, immediately before
            // the next model request.
            if self.has_steering() {
                self.deliver_steering(conversation, events).await?;

                continue;
            }

            if input_required {
                events.send(AgentEvent::TurnFinished {
                    turn,
                    reason: TurnFinish::InputRequired.label().to_owned(),
                });

                return Ok(TurnOutcome {
                    steps: step,
                    finish: TurnFinish::InputRequired,
                });
            }
        }

        events.send(AgentEvent::TurnFinished {
            turn,
            reason: TurnFinish::MaxSteps.label().to_owned(),
        });

        Ok(TurnOutcome {
            steps: step,
            finish: TurnFinish::MaxSteps,
        })
    }

    async fn deliver_steering(
        &self,
        conversation: &mut Conversation,
        events: &EventEmitter,
    ) -> Result<(), AgentError> {
        for text in self.take_steering() {
            // An empty steer is only a wake-up signal used to interrupt a turn;
            // it carries no user message and must not appear in the transcript.
            if text.trim().is_empty() {
                continue;
            }

            let message = Message::user_text(text.clone());

            self.journal
                .record_message(&message)
                .await
                .map_err(|error| AgentError::Journal(error.0))?;
            conversation.push(message);
            events.send(AgentEvent::UserMessage {
                text,
                source: crate::event::UserMessageSource::Steered,
                images: Vec::new(),
            });
        }

        Ok(())
    }

    /// Clone the run context with a reporter scoped to this call.
    ///
    /// The reporter forwards lossy UTF-8 chunks to `ToolProgress` until the
    /// context's output budget is spent, then stops; the final tool result is
    /// still the only thing the journal or the conversation sees.
    fn tool_context_for(&self, call: &ToolCall, events: &EventEmitter) -> ToolContext {
        let mut context = self.context.clone();

        // The copy a tool sees carries the agent's live mode, model, window, and
        // reasoning level. `spawn_agents` reads them from here, so a child starts
        // with what the parent has now rather than what it had at construction.
        context.mode = self.policy.mode();
        context.model.clone_from(&self.config.model);
        context.context_limit = self.config.context_limit;
        context.max_output_tokens = self.config.max_output_tokens;
        context.reasoning_effort = self.reasoning_effort();

        let call_id = call.id.as_str().to_owned();
        let limit = self.context.max_output_bytes;
        // stdout and stderr report concurrently, so the decoder and the byte
        // budget share one lock: a split character is held until its tail, and
        // the combined chunks never exceed the limit.
        let state = Arc::new(std::sync::Mutex::new((
            crate::tool::Utf8Stream::new(),
            0_usize,
        )));
        let events = events.clone();

        context.progress = Some(crate::tool::ProgressReporter::new(move |bytes: &[u8]| {
            if bytes.is_empty() {
                return;
            }

            let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
            let (stream, sent) = &mut *state;

            if *sent >= limit {
                return;
            }

            let text = stream.push(bytes);

            if text.is_empty() {
                return;
            }

            let mut take = text.len().min(limit - *sent);

            while take > 0 && !text.is_char_boundary(take) {
                take -= 1;
            }

            let chunk = text[..take].to_owned();

            *sent += take;

            // The event that reaches the budget announces that later chunks are
            // dropped; the frontend keeps whatever it already rendered.
            let truncated = *sent >= limit;

            drop(state);

            let _ = events.send(AgentEvent::ToolProgress {
                call_id: call_id.clone(),
                chunk,
                truncated,
            });
        }));

        context
    }

    /// Record a result for calls the turn never started.
    ///
    /// A failure to journal one marker must not leave the batch unanswered, so
    /// the marker is still pushed in memory and the first error is returned for
    /// the caller to report once the batch is closed.
    async fn mark_not_executed(
        &self,
        calls: &[ToolCall],
        reason: &str,
        results: &mut Vec<ToolResult>,
        events: &EventEmitter,
    ) -> Option<AgentError> {
        let mut failure = None;

        for call in calls {
            let result = ToolResult::not_executed(call, reason);

            if let Err(error) = self.journal.record_tool_result(&result).await {
                failure.get_or_insert(AgentError::Journal(error.0));
            }

            events.send(tool_result_event(&result, None));
            results.push(result);
        }

        failure
    }

    fn set_usage_baseline(&self, message_index: usize, tokens: u64) {
        *self
            .usage_baseline
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(UsageBaseline {
            message_index,
            tokens,
        });
    }

    fn clear_usage_baseline(&self) {
        *self
            .usage_baseline
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }

    fn estimated_context_tokens(
        &self,
        conversation: &Conversation,
        tools: &[crate::tool::ToolSpec],
    ) -> u64 {
        let projection = self.projection();
        let baseline = *self
            .usage_baseline
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        if let Some(baseline) = baseline {
            let covered = projection
                .as_ref()
                .map(|projection| projection.covered_messages)
                .unwrap_or(0);
            let messages = conversation.messages();

            if baseline.message_index >= covered && baseline.message_index <= messages.len() {
                return baseline
                    .tokens
                    .saturating_add(estimate_messages(&messages[baseline.message_index..]));
            }
        }

        estimate_projected(
            conversation.messages(),
            projection.as_ref(),
            tools,
            estimate_tokens(&self.config.system),
        )
    }

    fn should_compact(&self, conversation: &Conversation, tools: &[crate::tool::ToolSpec]) -> bool {
        let Some(limit) = self.config.context_limit else {
            return false;
        };

        ContextBudget::new(limit).should_compact(self.estimated_context_tokens(conversation, tools))
    }

    async fn compact(
        &self,
        conversation: &Conversation,
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let Some(limit) = self.config.context_limit else {
            return Ok(());
        };
        let previous = self.projection();
        let Some(compactor) = &self.compactor else {
            let error = AgentError::Compaction(
                "the estimated context reached its limit, but automatic compaction is not \
                 configured; no content was dropped"
                    .to_owned(),
            );

            return self.keep_projection_after_failure(previous.as_ref(), error, events);
        };

        let estimated = self.estimated_context_tokens(conversation, &self.tools.specs());
        let covered = previous
            .as_ref()
            .map(|projection| projection.covered_messages)
            .unwrap_or(0);
        let remaining = conversation.messages().get(covered..).unwrap_or_default();
        let Some(cut) = ContextBudget::new(limit).cut_point(remaining) else {
            // Nothing safe to summarize: the retention budget already covers the
            // whole remainder. That is not a failure, so the turn continues and
            // the provider bounds the request. Treating it as one would end a
            // small-window session's turn before its first request, because the
            // static system and tool estimate alone can reach the trigger.
            events.send(AgentEvent::Warning {
                message: "there is no safe compactable prefix: the recent context already fills\
                          the retention budget; continuing without compaction"
                    .to_owned(),
            });

            return Ok(());
        };
        let cut_index = covered + cut.cut;

        events.send(AgentEvent::CompactionStarted {
            estimated_tokens: estimated,
            context_limit: limit,
        });

        let sequence = self.compactions.fetch_add(1, Ordering::Relaxed);
        let request_prefix = format!("{}-compaction-{sequence}", self.config.request_namespace);
        let output = match self
            .summarize_compaction(
                compactor.as_ref(),
                conversation.messages(),
                previous.as_ref(),
                covered,
                cut,
                &request_prefix,
                limit,
                cancel,
            )
            .await
        {
            Ok(output) => output,
            Err(error) => {
                return self.keep_projection_after_failure(previous.as_ref(), error, events);
            }
        };

        for attempt in &output.attempts {
            self.journal
                .record_model_attempt(attempt)
                .await
                .map_err(|error| AgentError::Journal(error.0))?;
        }

        for usage in &output.usages {
            self.journal
                .record_usage(usage)
                .await
                .map_err(|error| AgentError::Journal(error.0))?;
            events.send(usage_event(usage));
        }

        let projection = Projection {
            summary: output.summary.clone(),
            covered_messages: cut_index,
        };

        self.journal
            .record_compaction(&CompactionRecord {
                summary: output.summary.clone(),
                covered_messages: cut_index,
                model: self.config.model.clone(),
                chunks: output.chunks,
            })
            .await
            .map_err(|error| AgentError::Journal(error.0))?;

        self.set_projection(Some(projection));
        self.clear_usage_baseline();

        events.send(AgentEvent::CompactionFinished {
            covered_messages: cut_index,
            summary_chars: output.summary.chars().count(),
        });

        Ok(())
    }

    /// A failed compaction is fatal only when there is no older summary to use.
    fn keep_projection_after_failure(
        &self,
        previous: Option<&Projection>,
        error: AgentError,
        events: &EventEmitter,
    ) -> Result<(), AgentError> {
        if previous.is_none() {
            return Err(error);
        }

        events.send(AgentEvent::Warning {
            message: format!("{error}; keeping the existing summary projection and continuing"),
        });

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn summarize_compaction(
        &self,
        compactor: &dyn ContextCompactor,
        messages: &[Message],
        previous: Option<&Projection>,
        covered: usize,
        cut: crate::context::CompactionCut,
        request_prefix: &str,
        limit: u64,
        cancel: &CancellationToken,
    ) -> Result<CompactionOutput, AgentError> {
        let history_end = covered + cut.history_end;
        let cut_index = covered + cut.cut;
        let mut summary = String::new();
        let mut chunks = 0_u32;
        let mut usages = Vec::new();
        let mut attempts = Vec::new();

        if cut.split_turn && history_end > covered {
            let mut segment = Vec::new();

            if let Some(previous) = previous {
                segment.push(summary_message(previous));
            }

            segment.extend_from_slice(&messages[covered..history_end]);

            let output = self
                .summarize_segment(
                    compactor,
                    segment,
                    format!("{request_prefix}-history"),
                    limit,
                    cancel,
                )
                .await?;

            summary = output.summary;
            chunks = output.chunks;
            usages = output.usages;
            attempts = output.attempts;
        }

        if cut.split_turn {
            let mut segment = Vec::new();

            if summary.is_empty()
                && let Some(previous) = previous
            {
                segment.push(summary_message(previous));
            }

            segment.extend_from_slice(&messages[history_end..cut_index]);

            let output = self
                .summarize_segment(
                    compactor,
                    segment,
                    format!("{request_prefix}-turn"),
                    limit,
                    cancel,
                )
                .await?;

            if summary.is_empty() {
                summary = output.summary;
            } else {
                summary.push_str("\n\n");
                summary.push_str(&output.summary);
            }

            chunks += output.chunks;
            usages.extend(output.usages);
            attempts.extend(output.attempts);
        } else {
            let mut segment = Vec::new();

            if let Some(previous) = previous {
                segment.push(summary_message(previous));
            }

            segment.extend_from_slice(&messages[covered..cut_index]);

            let output = self
                .summarize_segment(
                    compactor,
                    segment,
                    format!("{request_prefix}-history"),
                    limit,
                    cancel,
                )
                .await?;

            summary = output.summary;
            chunks = output.chunks;
            usages = output.usages;
            attempts = output.attempts;
        }

        Ok(CompactionOutput {
            summary,
            chunks,
            usages,
            attempts,
        })
    }

    async fn summarize_segment(
        &self,
        compactor: &dyn ContextCompactor,
        prefix: Vec<Message>,
        request_prefix: String,
        limit: u64,
        cancel: &CancellationToken,
    ) -> Result<CompactionOutput, AgentError> {
        let limits = crate::compaction::CompactionLimits {
            context_limit: limit,
            max_output_tokens: self.config.max_output_tokens,
        };

        match compactor
            .summarize(
                &prefix,
                &self.config.model,
                &self.config.agent_id,
                &request_prefix,
                limits,
                cancel,
            )
            .await
        {
            Ok(output) => Ok(output),
            Err(CompactionError::Cancelled) => Err(AgentError::Compaction(
                "context summarization was cancelled; the original context was retained".to_owned(),
            )),
            Err(CompactionError::Truncated) => Err(AgentError::Compaction(
                "context summarization was truncated by the model output limit; the original \
                 context was retained"
                    .to_owned(),
            )),
            Err(CompactionError::Failed(message)) => Err(AgentError::Compaction(format!(
                "{message}; the original context was retained"
            ))),
        }
    }

    fn request_messages(&self, conversation: &Conversation) -> Vec<Message> {
        match self.projection() {
            Some(projection) => {
                let mut messages = vec![Message::system_text(format!(
                    "[earlier conversation summary]\n{}",
                    projection.summary
                ))];
                messages.extend_from_slice(
                    conversation
                        .messages()
                        .get(
                            projection
                                .covered_messages
                                .min(conversation.messages().len())..,
                        )
                        .unwrap_or_default(),
                );

                messages
            }
            None => conversation.messages().to_vec(),
        }
    }

    fn check_image_support(
        &self,
        messages: &[Message],
        events: &EventEmitter,
    ) -> Result<(), AgentError> {
        if !messages.iter().any(Message::has_images) {
            return Ok(());
        }

        match self.config.supports_images {
            Some(false) => Err(AgentError::Provider(ProviderError::UnsupportedModel(
                format!(
                    "model `{}` does not accept image input; remove the attachment or choose an \
                     image-capable model",
                    self.config.model
                ),
            ))),
            None if !self.warned_unknown_images.swap(true, Ordering::Relaxed) => {
                events.send(AgentEvent::Warning {
                    message: format!(
                        "image capability for `{}` is unknown; attempting the request",
                        self.config.model
                    ),
                });

                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Validate a complete tool-call batch before it becomes conversation state.
fn invalid_batch(calls: &[ToolCall], finish: Option<&FinishReason>) -> Option<String> {
    match finish {
        Some(FinishReason::ToolCalls) => {}
        Some(_) => {
            return Some(
                "the provider finished a tool-call batch with a non-tool finish reason".to_owned(),
            );
        }
        None => {
            return Some("the provider stream ended without a tool_calls finish reason".to_owned());
        }
    }

    let mut seen = HashSet::with_capacity(calls.len());

    for call in calls {
        if !seen.insert(call.id.as_str()) {
            return Some(format!(
                "the provider repeated tool call id `{}` in one batch",
                call.id
            ));
        }
    }

    None
}

/// The tag of a whole reply made of fabricated tool markup, if any.
///
/// Some models answer with the literal `<tool_result>…</tool_result>` block
/// they wished a tool had produced. The text is kept and shown, but a
/// frontend is told it was mistaken for a result. A mention inside a longer
/// answer does not match.
fn fabricated_markup(text: &str) -> Option<&'static str> {
    let trimmed = text.trim();

    for tag in ["tool_result", "tool_call", "function_call"] {
        let open = format!("<{tag}");
        let Some(rest) = trimmed.strip_prefix(open.as_str()) else {
            continue;
        };

        if !rest.starts_with('>') && !rest.starts_with(char::is_whitespace) {
            continue;
        }

        let Some(attributes_end) = rest.find('>') else {
            continue;
        };

        if rest[..attributes_end].contains('<') {
            continue;
        }

        let close = format!("</{tag}>");

        if trimmed.ends_with(close.as_str()) {
            return Some(tag);
        }
    }

    None
}

fn tool_result_event(result: &ToolResult, process_id: Option<String>) -> AgentEvent {
    AgentEvent::ToolResult {
        call_id: result.call_id.as_str().to_owned(),
        tool: result.name.clone(),
        status: result.status,
        content: result.content.clone(),
        truncated: result.truncated,
        process_id,
    }
}

/// Why a call in a batch that a journal failure cut short never started.
fn journal_failed() -> &'static str {
    "not executed: the journal failed before this call started"
}

/// Answer every call a batch never reached.
///
/// The assistant message carrying all the calls is already in the conversation,
/// so each one needs a result or the next request is rejected. These markers are
/// not journaled: the journal is what failed.
fn answer_unreached(calls: &[ToolCall], results: &mut Vec<ToolResult>, reason: &str) {
    for call in calls {
        results.push(ToolResult::not_executed(call, reason));
    }
}

fn summary_message(projection: &Projection) -> Message {
    Message::system_text(format!(
        "Previous conversation summary:\n{}",
        projection.summary
    ))
}

fn usage_event(record: &UsageRecord) -> AgentEvent {
    AgentEvent::UsageRecorded {
        request_id: record.request_id.clone(),
        model: record.model.clone(),
        agent: record.agent.clone(),
        kind: record.kind.clone(),
        input_tokens: record.usage.input_tokens,
        output_tokens: record.usage.output_tokens,
        cost_usd: record.usage.cost_usd.clone(),
        generation_id: record
            .usage
            .generation_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        cache_read_tokens: record.usage.cache_read_tokens,
        cache_write_tokens: record.usage.cache_write_tokens,
        reasoning_tokens: record.usage.reasoning_tokens,
    }
}
