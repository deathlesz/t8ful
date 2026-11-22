//! Request-shape differences between OpenAI-compatible providers.

/// Where a provider asks for token accounting in a streamed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageStyle {
    /// OpenRouter: `"usage": { "include": true }`.
    Body,
    /// OpenAI-compatible: `"stream_options": { "include_usage": true }`.
    StreamOptions,
}

/// How a provider spells its reasoning request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningStyle {
    /// OpenRouter: `"reasoning": { "effort": "high" }` and
    /// `"reasoning": { "enabled": false }`.
    Object,
    /// OpenAI-compatible: `"reasoning_effort": "high"`. Turning reasoning off
    /// sends no field at all, because those endpoints have no `off` spelling.
    Flat,
}

/// Everything two OpenAI-compatible providers do differently in a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolProfile {
    /// Path appended to the provider base URL for a chat completion.
    pub chat_path: &'static str,
    pub usage_style: UsageStyle,
    pub reasoning_style: ReasoningStyle,
}

impl ProtocolProfile {
    pub const OPENROUTER: Self = Self {
        chat_path: "chat/completions",
        usage_style: UsageStyle::Body,
        reasoning_style: ReasoningStyle::Object,
    };

    pub const OPENCODE_GO: Self = Self {
        chat_path: "openai/v1/chat/completions",
        usage_style: UsageStyle::StreamOptions,
        reasoning_style: ReasoningStyle::Flat,
    };
}

#[cfg(test)]
mod tests {
    use super::ProtocolProfile;

    #[test]
    fn the_provider_profiles_keep_their_distinct_paths() {
        assert_eq!(ProtocolProfile::OPENROUTER.chat_path, "chat/completions");
        assert_eq!(
            ProtocolProfile::OPENCODE_GO.chat_path,
            "openai/v1/chat/completions"
        );
    }
}
