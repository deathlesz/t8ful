use serde::{Deserialize, Serialize};

use crate::ids::GenerationId;

/// Provider-reported usage for one generation.
///
/// Every field is optional on purpose: a missing value stays `None` and is
/// reported as unknown, never as zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,

    /// Prompt tokens served from a provider cache, when reported separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,

    /// Prompt tokens written to a provider cache, when reported separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,

    /// Reasoning tokens the provider counted. A subset of the output count, so
    /// it is displayed but never added to [`Usage::context_tokens`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,

    /// Decimal string so no currency precision is lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<GenerationId>,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cache_read_tokens.is_none()
            && self.cache_write_tokens.is_none()
            && self.reasoning_tokens.is_none()
            && self.cost_usd.is_none()
            && self.generation_id.is_none()
    }

    pub fn total_tokens(&self) -> Option<u64> {
        match (self.input_tokens, self.output_tokens) {
            (None, None) => None,
            (input, output) => Some(input.unwrap_or(0) + output.unwrap_or(0)),
        }
    }

    /// Tokens the provider counted for the request that produced this usage.
    ///
    /// This is the baseline for context compaction. The input count is the
    /// provider's prompt token count, and the cache counts are subsets of it, so
    /// the prompt is counted once and only the output is added: adding the cache
    /// fields would inflate the baseline of every cached prompt. A usage record
    /// without an input count is not a usable baseline.
    pub fn context_tokens(&self) -> Option<u64> {
        let input = self.input_tokens?;

        Some(input.saturating_add(self.output_tokens.unwrap_or(0)))
    }

    /// Fill in fields this record does not know, keeping known values intact.
    pub fn fill_from(&mut self, other: &Self) {
        if self.input_tokens.is_none() {
            self.input_tokens = other.input_tokens;
        }

        if self.output_tokens.is_none() {
            self.output_tokens = other.output_tokens;
        }

        if self.cache_read_tokens.is_none() {
            self.cache_read_tokens = other.cache_read_tokens;
        }

        if self.cache_write_tokens.is_none() {
            self.cache_write_tokens = other.cache_write_tokens;
        }

        if self.reasoning_tokens.is_none() {
            self.reasoning_tokens = other.reasoning_tokens;
        }

        if self.cost_usd.is_none() {
            self.cost_usd.clone_from(&other.cost_usd);
        }

        if self.generation_id.is_none() {
            self.generation_id.clone_from(&other.generation_id);
        }
    }
}
