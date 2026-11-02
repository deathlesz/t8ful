use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::content::ImageInput;
use crate::policy::Mode;
use crate::provider::ReasoningEffort;
use crate::tool::ToolStatus;

/// Where a user message came from. Derived from the driver's intake path, not
/// from the model: a submitted prompt, an in-turn steer, or a queued follow-up.
/// Child reports are injected directly into the conversation and announce
/// themselves with [`AgentEvent::AgentNoticeDelivered`], not as user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserMessageSource {
    Submitted,
    Steered,
    Queued,
}

/// Role of one restored transcript entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRole {
    User,
    Assistant,
    Tool,
    Info,
}

/// One entry from an existing session, rendered by a frontend on resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub role: HistoryRole,
    pub text: String,
    /// Images that belonged to this message, in their original order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageInput>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
    #[serde(default)]
    pub status: Option<ToolStatus>,
    #[serde(default)]
    pub truncated: bool,
}

/// Everything a frontend or store can observe about a turn.
///
/// The TUI, the headless renderers, and the future session store consume this
/// same stream, so variants are part of the public schema: keep tag and field
/// names stable and additive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStarted {
        turn: u64,
    },
    ModelStarted {
        model: String,
        step: u32,
    },
    /// Display-only reasoning text from the provider. It is never written to
    /// the journal or inserted into the conversation.
    ReasoningDelta {
        text: String,
    },
    TextDelta {
        text: String,
    },
    TextCompleted {
        text: String,
    },
    ToolCallStarted {
        call_id: String,
        tool: String,
        arguments: serde_json::Value,
    },
    /// Display-only output captured while a tool is still running. It is never
    /// journaled; the final `ToolResult` remains authoritative.
    ToolProgress {
        call_id: String,
        chunk: String,
        truncated: bool,
    },
    ToolResult {
        call_id: String,
        tool: String,
        status: ToolStatus,
        content: String,
        truncated: bool,
        /// The managed process this result started, when the tool launched one.
        /// A frontend can bind later display-only progress to the same card.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process_id: Option<String>,
    },
    /// A user-authored message that entered the conversation. Emitted after the
    /// journal append so every frontend can echo it exactly once.
    UserMessage {
        text: String,
        source: UserMessageSource,
        /// Images submitted with this message. A frontend uses them to replace
        /// its pending tray entry and to render the transcript block.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageInput>,
    },
    /// A child agent started. The dashboard counts it from here.
    AgentSpawned {
        agent: String,
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        isolation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<String>,
    },
    /// One event produced by a child agent, wrapped so a frontend can attribute
    /// it without the root transcript absorbing the child's output.
    AgentEvent {
        agent: String,
        event: Box<AgentEvent>,
    },
    /// A child agent stopped, with the report its parent received.
    AgentFinished {
        agent: String,
        role: String,
        status: String,
        summary: String,
        steps: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        elapsed_ms: u64,
    },
    /// A completion notice entered the parent's conversation.
    AgentNoticeDelivered {
        agent: String,
    },
    /// A background-process notice entered the conversation.
    ProcessNoticeDelivered {
        process_id: String,
    },
    /// The task graph changed. Emitted after the journal append.
    TaskChanged {
        id: String,
        title: String,
        state: String,
        blocked: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
    },
    /// A skill loaded on explicit user request. It carries the same body the
    /// `skill` tool returns so every activation path renders one card.
    SkillLoaded {
        name: String,
        content: String,
        truncated: bool,
    },
    ProcessStarted {
        process_id: String,
        command: String,
    },
    ProcessStatus {
        process_id: String,
        state: String,
        exit_code: Option<i32>,
    },
    ProcessOutput {
        process_id: String,
        chunk: String,
        truncated: bool,
    },
    ProcessSnapshot {
        entries: Vec<String>,
    },
    /// Display-only output from a managed background process. It is never
    /// journaled or added to model context; the final tool result, an explicit
    /// `process_output`, and the completion notice remain authoritative.
    ProcessProgress {
        process_id: String,
        chunk: String,
        truncated: bool,
        /// `chunk` replaces the display buffer instead of appending, which lets
        /// a slow frontend recover from a dropped update.
        #[serde(default)]
        reset: bool,
    },
    UsageRecorded {
        #[serde(default)]
        request_id: String,
        model: String,
        #[serde(default)]
        agent: String,
        #[serde(default)]
        kind: String,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost_usd: Option<String>,
        generation_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_write_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_tokens: Option<u64>,
    },
    Warning {
        message: String,
    },
    ModeChanged {
        mode: Mode,
        /// The mode the switch left, set only for a user-initiated change.
        /// Startup, resume, and session syncs leave it unset so a frontend
        /// never draws a divider for a mode the user did not pick.
        #[serde(default)]
        previous: Option<Mode>,
    },
    ModelChanged {
        model: String,
        #[serde(default)]
        supports_reasoning: Option<bool>,
    },
    ReasoningEffortChanged {
        effort: ReasoningEffort,
    },
    CompactionStarted {
        estimated_tokens: u64,
        context_limit: u64,
    },
    CompactionFinished {
        covered_messages: usize,
        summary_chars: usize,
    },
    AttachmentAdded {
        path: String,
        media_type: String,
        width: u32,
        height: u32,
        bytes: u64,
    },
    QueueUpdated {
        items: Vec<String>,
        paused: bool,
    },
    PlanPresented {
        plan: String,
    },
    SessionChanged {
        session_id: String,
    },
    HistoryRestored {
        entries: Vec<HistoryEntry>,
    },
    TurnFinished {
        turn: u64,
        reason: String,
    },
    Error {
        message: String,
    },
    InputRequired {
        kind: String,
        detail: String,
    },
}

/// An event with a per-run monotonic sequence number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequencedEvent {
    pub seq: u64,
    pub event: AgentEvent,
}

/// Producer side of the event stream.
///
/// Cloning shares the sequence counter, so a reporter handed to a running tool
/// keeps the stream monotonic.
#[derive(Debug, Clone)]
pub struct EventEmitter {
    sender: mpsc::UnboundedSender<SequencedEvent>,
    next_seq: Arc<AtomicU64>,
}

impl EventEmitter {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<SequencedEvent>) {
        let (sender, receiver) = mpsc::unbounded_channel();

        (
            Self {
                sender,
                next_seq: Arc::new(AtomicU64::new(1)),
            },
            receiver,
        )
    }

    /// Send an event. Returns `false` when the consumer has gone away.
    pub fn send(&self, event: AgentEvent) -> bool {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);

        self.sender.send(SequencedEvent { seq, event }).is_ok()
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}
