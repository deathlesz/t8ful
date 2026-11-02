//! Durable journal hooks used by the agent loop.
//!
//! The agent calls these hooks at the dispatch boundary. An append failure is
//! fatal for the turn: there is no memory-only fallback, because that would let
//! a crash replay a side effect that was never recorded.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::message::Message;
use crate::tool::{ToolCall, ToolResult};
use crate::usage::Usage;

#[derive(Debug, thiserror::Error)]
#[error("session journal append failed: {0}")]
pub struct JournalError(pub String);

impl JournalError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAttempt {
    pub request_id: String,
    pub model: String,
    pub agent: String,
    pub step: u32,
    pub max_output_tokens: Option<u32>,
    pub message_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub request_id: String,
    pub model: String,
    pub agent: String,
    /// `turn`, `compaction`, or another provider-call purpose.
    pub kind: String,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub summary: String,
    /// Number of conversation messages covered by the summary projection.
    pub covered_messages: usize,
    pub model: String,
    pub chunks: u32,
}

#[async_trait]
pub trait AgentJournal: Send + Sync {
    async fn record_message(&self, message: &Message) -> Result<(), JournalError>;

    async fn record_model_attempt(&self, attempt: &ModelAttempt) -> Result<(), JournalError>;

    async fn record_dispatch(&self, call: &ToolCall) -> Result<(), JournalError>;

    async fn record_tool_result(&self, result: &ToolResult) -> Result<(), JournalError>;

    async fn record_usage(&self, usage: &UsageRecord) -> Result<(), JournalError>;

    async fn record_compaction(&self, projection: &CompactionRecord) -> Result<(), JournalError>;
}

/// No-op journal used in unit tests and by callers that do not keep a session.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullJournal;

#[async_trait]
impl AgentJournal for NullJournal {
    async fn record_message(&self, _message: &Message) -> Result<(), JournalError> {
        Ok(())
    }

    async fn record_model_attempt(&self, _attempt: &ModelAttempt) -> Result<(), JournalError> {
        Ok(())
    }

    async fn record_dispatch(&self, _call: &ToolCall) -> Result<(), JournalError> {
        Ok(())
    }

    async fn record_tool_result(&self, _result: &ToolResult) -> Result<(), JournalError> {
        Ok(())
    }

    async fn record_usage(&self, _usage: &UsageRecord) -> Result<(), JournalError> {
        Ok(())
    }

    async fn record_compaction(&self, _projection: &CompactionRecord) -> Result<(), JournalError> {
        Ok(())
    }
}
