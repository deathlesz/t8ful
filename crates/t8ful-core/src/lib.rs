//! Core contracts shared by every t8ful frontend, provider, and tool.
//!
//! This crate deliberately knows nothing about terminals, HTTP, or process
//! management. It owns the normalized conversation model, the provider and tool
//! interfaces, the central policy gate, the typed event stream, context
//! budgeting, and the agent loop. Frontends consume [`event::AgentEvent`];
//! providers and tools implement the traits defined here.

pub mod agent;
pub mod compaction;
pub mod content;
pub mod context;
pub mod error;
pub mod event;
pub mod ids;
pub mod journal;
pub mod message;
pub mod notice;
pub mod policy;
pub mod provider;
pub mod question;
pub mod readscope;
pub mod sandbox;
pub mod subagent;
pub mod task;
pub mod tool;
pub mod usage;
pub mod worktree;

pub use agent::{Agent, AgentConfig, TurnFinish, TurnOutcome};
pub use compaction::{CompactionError, CompactionOutput, ContextCompactor};
pub use content::{Content, ImageInput};
pub use context::{
    COMPACTION_KEEP_RECENT_TOKENS, COMPACTION_RESERVE_TOKENS, CompactionCut, ContextBudget,
    DEFAULT_CONTEXT_LIMIT_FALLBACK, DEFAULT_OUTPUT_RESERVE_PERCENT, Projection, compaction_cut,
    estimate_messages, estimate_projected, estimate_tokens, estimate_tools, resolve_output_budget,
    turn_ranges,
};
pub use error::{AgentError, ProviderError};
pub use event::{AgentEvent, EventEmitter, SequencedEvent};
pub use ids::{CallId, GenerationId};
pub use journal::{
    AgentJournal, CompactionRecord, JournalError, ModelAttempt, NullJournal, UsageRecord,
};
pub use message::{Conversation, Message, Role};
pub use notice::{
    AgentNoticeRow, Notice, NoticeRow, PROCESS_NOTICE_TAIL, ProcessNotice, ProcessNoticeRow,
    parse_notice,
};
pub use policy::{Decision, Mode, Policy};
pub use provider::{
    FinishReason, ModelInfo, Provider, ProviderEvent, ProviderRequest, ProviderStream,
    ReasoningEffort, UnavailableProvider,
};
pub use question::{
    FixedQuestionRouter, Question, QuestionAnswer, QuestionOption, QuestionOutcome,
    QuestionRequest, QuestionRouter,
};
pub use readscope::ReadScope;
pub use sandbox::{Sandbox, SandboxError, SandboxedCommand, SandboxedProcess, ToolSource};
pub use subagent::{
    AgentId, AgentLimits, AgentNotice, AgentRegistry, AgentReport, ChildAgent, ChildFactory,
    ChildSnapshot, ChildStatus, Isolation, NoticeQueue, RoleProfile, SpawnError, SpawnRequest,
    Worktree,
};
pub use task::{Task, TaskError, TaskGraph, TaskId, TaskRecord, TaskRow, TaskState};
pub use tool::{
    Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolRegistry, ToolResult,
    ToolSpec, ToolStatus,
};
pub use usage::Usage;
pub use worktree::{NoWorktrees, WorktreeError, Worktrees};

/// Default maximum base64-encoded image payload accepted by `/attach`.
pub const DEFAULT_MAX_IMAGE_ENCODED_BYTES: usize = 20 * 1024 * 1024;
