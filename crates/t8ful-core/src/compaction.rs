//! Summary compaction behind a core-facing interface.
//!
//! The compactor uses the selected model with tools disabled. It either returns
//! a complete summary or fails; the agent only commits a projection after every
//! chunk succeeded.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::journal::{ModelAttempt, UsageRecord};
use crate::message::Message;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionOutput {
    pub summary: String,
    pub chunks: u32,
    pub usages: Vec<UsageRecord>,
    pub attempts: Vec<ModelAttempt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionLimits {
    pub context_limit: u64,
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum CompactionError {
    #[error("context summary failed: {0}")]
    Failed(String),

    #[error("context summary was cancelled")]
    Cancelled,

    #[error("context summary was truncated by the model output limit")]
    Truncated,
}

#[async_trait]
pub trait ContextCompactor: Send + Sync {
    /// Summarize a complete, already validated conversation prefix.
    async fn summarize(
        &self,
        prefix: &[Message],
        model: &str,
        agent: &str,
        request_prefix: &str,
        limits: CompactionLimits,
        cancel: &CancellationToken,
    ) -> Result<CompactionOutput, CompactionError>;
}
