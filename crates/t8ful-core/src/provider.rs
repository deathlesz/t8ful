use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;
use futures_util::stream;
use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::message::Message;
use crate::tool::{ToolCall, ToolSpec};
use crate::usage::Usage;

#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: Option<u32>,
    pub reasoning: ReasoningEffort,
    /// Provider-specific value for `reasoning` when the model maps this level
    /// to something other than the common label.
    pub reasoning_value: Option<String>,
    /// The conversation this request belongs to.
    ///
    /// Providers that need a per-conversation identifier on the wire (OpenCode
    /// Go's `x-opencode-session`) derive it from here. It is never sent as part
    /// of the message content.
    pub session_id: Option<String>,
}

/// Requested reasoning effort. `Default` sends no reasoning field at all.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    #[default]
    Default,
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningEffort {
    pub const ALL: [Self; 8] = [
        Self::Default,
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "default" => Self::Default,
            "off" => Self::Off,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" | "x-high" | "x_high" => Self::XHigh,
            "max" => Self::Max,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// The reasoning levels one model actually offers.
///
/// This follows Pi: a common vocabulary (`off` through `max`), narrowed by an
/// optional per-model map whose entries either name the provider value for a
/// level or mark the level unsupported. Without a map, reasoning models offer
/// `off` through `high` and the extended levels stay unsupported.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReasoningProfile {
    available: Vec<ReasoningEffort>,
    mapped: std::collections::BTreeMap<ReasoningEffort, Option<String>>,
}

impl ReasoningProfile {
    /// Levels offered when nothing is known about a model.
    pub fn unknown() -> Self {
        Self::from_support(None)
    }

    pub fn from_support(supports_reasoning: Option<bool>) -> Self {
        let available = match supports_reasoning {
            Some(false) => vec![ReasoningEffort::Default, ReasoningEffort::Off],
            Some(true) | None => vec![
                ReasoningEffort::Default,
                ReasoningEffort::Off,
                ReasoningEffort::Minimal,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
        };

        Self {
            available,
            mapped: std::collections::BTreeMap::new(),
        }
    }

    /// Build a profile from the efforts a provider advertises for a model.
    ///
    /// OpenRouter reports `supported_efforts` per model, so the picker can show
    /// exactly what that model accepts instead of a fixed menu. The advertised
    /// spelling `none` means "no reasoning", which t8ful calls `off`. t8ful's
    /// `default` is always offered because it sends no reasoning field at all.
    pub fn from_efforts(efforts: &[String], mandatory: bool) -> Self {
        let mut profile = Self {
            available: vec![ReasoningEffort::Default],
            mapped: std::collections::BTreeMap::new(),
        };

        for effort in efforts {
            let level = match ReasoningEffort::parse(effort) {
                Some(ReasoningEffort::Default) | None => match effort.trim() {
                    "none" => ReasoningEffort::Off,
                    _ => continue,
                },
                Some(level) => level,
            };

            if !profile.available.contains(&level) {
                profile.available.push(level);
            }
        }

        // A mandatory model always reasons, so `off` stays unavailable even
        // though t8ful could ask for it.
        if mandatory {
            profile
                .available
                .retain(|level| *level != ReasoningEffort::Off);
        }

        profile.sort_available();

        profile
    }

    /// Record the provider value for one level, or `None` to hide it.
    ///
    /// `Default` means "send nothing", so an override for it is ignored.
    pub fn with_mapped(mut self, effort: ReasoningEffort, value: Option<String>) -> Self {
        if effort == ReasoningEffort::Default {
            return self;
        }

        if value.is_some() {
            if !self.available.contains(&effort) {
                self.available.push(effort);
            }
        } else {
            self.available.retain(|level| *level != effort);
        }

        self.mapped.insert(effort, value);
        self.sort_available();

        self
    }

    fn sort_available(&mut self) {
        self.available.sort_by_key(|effort| {
            ReasoningEffort::ALL
                .iter()
                .position(|candidate| candidate == effort)
                .unwrap_or_default()
        });
    }

    pub fn levels(&self) -> Vec<ReasoningEffort> {
        self.available.clone()
    }

    pub fn supports(&self, effort: ReasoningEffort) -> bool {
        self.available.contains(&effort)
    }

    /// The provider value for a level, when the model overrides the default.
    pub fn provider_value(&self, effort: ReasoningEffort) -> Option<String> {
        self.mapped.get(&effort).cloned().flatten()
    }

    /// The closest supported level: prefer a stronger one, then a weaker one.
    pub fn clamp(&self, effort: ReasoningEffort) -> ReasoningEffort {
        if self.supports(effort) {
            return effort;
        }

        let start = ReasoningEffort::ALL
            .iter()
            .position(|candidate| *candidate == effort)
            .unwrap_or_default();

        for candidate in ReasoningEffort::ALL.iter().skip(start) {
            if self.supports(*candidate) {
                return *candidate;
            }
        }

        for candidate in ReasoningEffort::ALL.iter().take(start).rev() {
            if self.supports(*candidate) {
                return *candidate;
            }
        }

        ReasoningEffort::Default
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    /// Display-only reasoning text. It never enters the conversation.
    ReasoningDelta(String),
    TextDelta(String),
    ToolCallComplete(ToolCall),
    Usage(Usage),
    Finished(FinishReason),
    /// A display-only notice from the transport, such as a retry it is about to
    /// wait out. It never enters the conversation and is never journaled.
    Warning(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    /// The provider this model came from, set by the layer that composed the
    /// catalog (for example `openrouter` or `opencode-go`).
    ///
    /// Display and routing metadata only: it is never sent to the API.
    pub provider: Option<String>,
    pub name: Option<String>,
    pub context_length: Option<u64>,
    /// Provider-advertised maximum output tokens, when known.
    pub max_output_tokens: Option<u64>,
    /// `Some(false)` means the model is known not to accept images.
    pub supports_images: Option<bool>,
    /// `Some(false)` means the model does not advertise reasoning parameters.
    pub supports_reasoning: Option<bool>,
    /// The reasoning levels the model offers, when a provider or configuration
    /// overrides the defaults derived from `supports_reasoning`.
    pub reasoning: Option<ReasoningProfile>,
    pub prompt_price: Option<String>,
    pub completion_price: Option<String>,
}

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send>>;

#[async_trait]
pub trait Provider: Send + Sync {
    /// Discover models the account can use.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Discover metadata for one model, when the provider can do so cheaply.
    /// The default reports that metadata is unavailable.
    async fn model_info(&self, _model: &str) -> Result<Option<ModelInfo>, ProviderError> {
        Ok(None)
    }

    /// Start a streamed completion.
    ///
    /// Dropping the returned stream cancels the request; the agent records any
    /// already dispatched tool call as indeterminate instead of replaying it.
    fn stream(&self, request: ProviderRequest) -> ProviderStream;
}

/// Provider used by tests and by callers that only need core behavior.
pub struct UnavailableProvider;

#[async_trait]
impl Provider for UnavailableProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Err(ProviderError::Other(
            "no provider is configured for this operation".to_owned(),
        ))
    }

    fn stream(&self, _request: ProviderRequest) -> ProviderStream {
        Box::pin(stream::once(async {
            Err(ProviderError::Other(
                "no provider is configured for this operation".to_owned(),
            ))
        }))
    }
}

#[cfg(test)]
mod reasoning_profile_tests {
    use super::{ReasoningEffort, ReasoningProfile};

    #[test]
    fn support_flags_and_custom_maps_select_the_available_levels() {
        for (support, expected) in [
            (
                Some(false),
                vec![ReasoningEffort::Default, ReasoningEffort::Off],
            ),
            (
                Some(true),
                vec![
                    ReasoningEffort::Default,
                    ReasoningEffort::Off,
                    ReasoningEffort::Minimal,
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                ],
            ),
        ] {
            assert_eq!(ReasoningProfile::from_support(support).levels(), expected);
        }

        let unknown = ReasoningProfile::from_support(None);

        assert!(unknown.supports(ReasoningEffort::High));
        assert!(!unknown.supports(ReasoningEffort::XHigh));

        let mapped = ReasoningProfile::from_support(Some(true))
            .with_mapped(ReasoningEffort::Minimal, None)
            .with_mapped(ReasoningEffort::High, Some("high".to_owned()))
            .with_mapped(ReasoningEffort::Max, Some("max".to_owned()));

        assert_eq!(
            mapped.levels(),
            vec![
                ReasoningEffort::Default,
                ReasoningEffort::Off,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ]
        );
        assert_eq!(
            mapped.provider_value(ReasoningEffort::High).as_deref(),
            Some("high")
        );
        assert_eq!(mapped.provider_value(ReasoningEffort::Low), None);

        let upward = ReasoningProfile::from_support(Some(true))
            .with_mapped(ReasoningEffort::Minimal, None)
            .with_mapped(ReasoningEffort::Low, None)
            .with_mapped(ReasoningEffort::Medium, None)
            .with_mapped(ReasoningEffort::High, None)
            .with_mapped(ReasoningEffort::XHigh, Some("xhigh".to_owned()));

        assert_eq!(
            upward.clamp(ReasoningEffort::Medium),
            ReasoningEffort::XHigh
        );

        let narrow = ReasoningProfile::from_support(Some(true))
            .with_mapped(ReasoningEffort::Low, None)
            .with_mapped(ReasoningEffort::Medium, None)
            .with_mapped(ReasoningEffort::High, None);

        assert_eq!(
            narrow.clamp(ReasoningEffort::High),
            ReasoningEffort::Minimal
        );
        assert_eq!(
            narrow.clamp(ReasoningEffort::Default),
            ReasoningEffort::Default
        );
    }

    #[test]
    fn advertised_efforts_define_the_levels_and_off() {
        let all = |entries: &[&str], mandatory| {
            ReasoningProfile::from_efforts(
                &entries
                    .iter()
                    .map(|entry| (*entry).to_owned())
                    .collect::<Vec<_>>(),
                mandatory,
            )
        };

        // `none` is the provider spelling for t8ful's `off`.
        let full = all(&["max", "xhigh", "high", "medium", "low", "none"], false);

        assert_eq!(
            full.levels(),
            vec![
                ReasoningEffort::Default,
                ReasoningEffort::Off,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
                ReasoningEffort::Max,
            ]
        );
        assert!(full.supports(ReasoningEffort::XHigh));
        assert!(full.supports(ReasoningEffort::Max));
        assert!(!full.supports(ReasoningEffort::Minimal));

        // OpenRouter reports mandatory models without `none`.
        let mandatory = all(&["max", "xhigh", "high", "medium", "low"], true);

        assert_eq!(
            mandatory.levels(),
            vec![
                ReasoningEffort::Default,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
                ReasoningEffort::Max,
            ]
        );
        assert!(!mandatory.supports(ReasoningEffort::Off));

        assert_eq!(
            all(&["high", "low"], true).levels(),
            vec![
                ReasoningEffort::Default,
                ReasoningEffort::Low,
                ReasoningEffort::High
            ]
        );

        let with_off = all(&["high", "low", "none"], false);

        assert!(with_off.supports(ReasoningEffort::Off));
        assert_eq!(
            with_off.levels(),
            vec![
                ReasoningEffort::Default,
                ReasoningEffort::Off,
                ReasoningEffort::Low,
                ReasoningEffort::High
            ]
        );
    }
}
