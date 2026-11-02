//! Context budgeting and compaction boundaries.
//!
//! Token counts are estimates. They drive a conservative compaction trigger, not
//! an exact accounting: provider rejection is reported instead of being hidden
//! by dropping content.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::message::{Message, Role};
use crate::tool::ToolSpec;

pub const DEFAULT_CONTEXT_LIMIT_FALLBACK: u64 = 16_384;
pub const DEFAULT_OUTPUT_RESERVE_PERCENT: u64 = 20;

/// Headroom kept free so the next request cannot immediately overflow.
pub const COMPACTION_RESERVE_TOKENS: u64 = 16_384;

/// Estimated size of the recent suffix kept verbatim across a compaction.
pub const COMPACTION_KEEP_RECENT_TOKENS: u64 = 20_000;

/// A summary projection over an unchanged on-disk conversation prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projection {
    pub summary: String,
    pub covered_messages: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub context_limit: u64,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

/// Split a context window into the reserve and the retained suffix.
///
/// The nominal constants are absolute budgets: a window large enough for both
/// keeps them unchanged, so compaction behaves the same at every normal size.
/// A window too small for them - the catalog-discovery fallback is the common
/// case - scales both to the window instead of disabling compaction, because a
/// session that cannot compact runs until the provider rejects the request.
fn reserve_and_suffix(context_limit: u64) -> (u64, u64) {
    if context_limit > COMPACTION_RESERVE_TOKENS.saturating_add(COMPACTION_KEEP_RECENT_TOKENS) {
        return (COMPACTION_RESERVE_TOKENS, COMPACTION_KEEP_RECENT_TOKENS);
    }

    // A quarter of the window is held back as the trigger reserve and the
    // suffix takes at most half of the remainder. The reserve is a trigger
    // threshold, not a guaranteed free window: the cut walks back to a user or
    // assistant boundary, so one large tool result can carry the suffix past
    // its budget.
    let reserve = (context_limit / 4).max(1);
    let keep = (context_limit.saturating_sub(reserve) / 2).max(1);

    (reserve, keep)
}

impl ContextBudget {
    pub fn new(context_limit: u64) -> Self {
        let context_limit = context_limit.max(1);
        let (reserve_tokens, keep_recent_tokens) = reserve_and_suffix(context_limit);

        Self {
            context_limit,
            reserve_tokens,
            keep_recent_tokens,
        }
    }

    /// Whether the context window can hold the reserve and the retained suffix.
    ///
    /// The budgets shrink to fit an unusually small window, so this only refuses
    /// a window with no room to compact at all, which would otherwise
    /// re-summarize the same messages forever.
    pub fn can_compact(&self) -> bool {
        self.context_limit > self.reserve_tokens.saturating_add(self.keep_recent_tokens)
    }

    /// Compaction starts when the next request could consume the reserve.
    pub fn should_compact(&self, estimated_tokens: u64) -> bool {
        self.can_compact()
            && estimated_tokens.saturating_add(self.reserve_tokens) >= self.context_limit
    }

    /// Choose the boundary between the summarized prefix and the kept suffix.
    ///
    /// Walk backward until the recent suffix reaches the retention budget, then
    /// move to the nearest user or assistant message. A tool result is never a
    /// cut point because it must stay with its tool call.
    pub fn cut_point(&self, messages: &[Message]) -> Option<CompactionCut> {
        compaction_cut(messages, self.keep_recent_tokens)
    }
}

/// The boundary selected for one compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionCut {
    /// Start of the verbatim suffix; this becomes `covered_messages`.
    pub cut: usize,
    /// End of the ordinary history segment. Equal to `cut` unless the cut
    /// splits a user turn.
    pub history_end: usize,
    /// Whether the cut lands inside the newest user turn.
    pub split_turn: bool,
}

/// Find the boundary between summarized history and the verbatim suffix.
///
/// The returned cut is always a user or assistant message and always leaves at
/// least one message in the summarized prefix.
pub fn compaction_cut(messages: &[Message], keep_recent_tokens: u64) -> Option<CompactionCut> {
    if messages.is_empty() {
        return None;
    }

    let mut used = 0_u64;
    let mut cut = messages.len();

    for index in (0..messages.len()).rev() {
        used = used.saturating_add(estimate_message(&messages[index]));
        cut = index;

        if used >= keep_recent_tokens {
            break;
        }
    }

    if cut == 0 {
        return None;
    }

    while cut > 0 && !matches!(messages[cut].role, Role::User | Role::Assistant) {
        cut -= 1;
    }

    if cut == 0 || !matches!(messages[cut].role, Role::User | Role::Assistant) {
        return None;
    }

    let prefix = &messages[..cut];
    let prefix_has_work = prefix.iter().any(|message| {
        message.role == Role::User || message.role == Role::Assistant || message.role == Role::Tool
    });

    if !prefix_has_work {
        return None;
    }

    let split_turn = messages[cut].role == Role::Assistant
        && prefix.iter().any(|message| message.role == Role::User);
    let history_end = if split_turn {
        prefix
            .iter()
            .rposition(|message| message.role == Role::User)
            .unwrap_or(0)
    } else {
        cut
    };

    Some(CompactionCut {
        cut,
        history_end,
        split_turn: history_end < cut,
    })
}

/// Estimate the request size from a projection over the conversation.
pub fn estimate_projected(
    messages: &[Message],
    projection: Option<&Projection>,
    tools: &[ToolSpec],
    system_tokens: u64,
) -> u64 {
    let (remaining, summary_tokens) = match projection {
        Some(projection) => (
            messages
                .get(projection.covered_messages.min(messages.len())..)
                .unwrap_or_default(),
            estimate_tokens(&projection.summary),
        ),
        None => (messages, 0),
    };

    estimate_messages(remaining)
        .saturating_add(estimate_tools(tools))
        .saturating_add(summary_tokens)
        .saturating_add(system_tokens)
}

/// Resolve the model output budget.
///
/// The default is 20% of the context window, capped by model metadata. An
/// explicit value is never silently changed: values above the context window or
/// the provider maximum are errors.
pub fn resolve_output_budget(
    explicit: Option<u32>,
    context_limit: u64,
    advertised_maximum: Option<u64>,
) -> Result<u32, String> {
    if context_limit == 0 {
        return Err("the model context limit must be greater than zero".to_owned());
    }

    let default = context_limit.saturating_mul(DEFAULT_OUTPUT_RESERVE_PERCENT) / 100;
    let default = default.max(1);

    match explicit {
        Some(0) => Err("agent.max_output_tokens must be at least 1".to_owned()),
        Some(value) => {
            let value = u64::from(value);

            if value > context_limit {
                return Err(format!(
                    "agent.max_output_tokens = {value} exceeds the model context window of \
                     {context_limit} tokens"
                ));
            }

            if let Some(maximum) = advertised_maximum
                && value > maximum
            {
                return Err(format!(
                    "agent.max_output_tokens = {value} exceeds the model's advertised \
                         maximum of {maximum} output tokens"
                ));
            }

            Ok(u32::try_from(value).unwrap_or(u32::MAX))
        }
        None => {
            let value = advertised_maximum
                .map(|max| default.min(max))
                .unwrap_or(default)
                .max(1);

            Ok(u32::try_from(value).unwrap_or(u32::MAX))
        }
    }
}

pub fn estimate_tokens(text: &str) -> u64 {
    // Four UTF-8 bytes per token, rounded up, plus a small overhead.
    (text.len() as u64).div_ceil(4)
}

pub fn estimate_message(message: &Message) -> u64 {
    let mut tokens = 4;
    let mut has_image = false;

    for item in &message.content {
        match item {
            crate::content::Content::Text { text } => tokens += estimate_tokens(text),
            crate::content::Content::Image(image) => {
                has_image = true;
                tokens += estimate_image_tokens(image.width, image.height);
            }
            crate::Content::ToolCall(call) => {
                tokens +=
                    estimate_tokens(&call.name) + estimate_tokens(&call.arguments.to_string());
            }
            crate::Content::ToolResult(result) => {
                tokens += estimate_tokens(&result.content);
            }
        }
    }

    if has_image {
        tokens += 8;
    }

    tokens
}

pub fn estimate_messages(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_message).sum()
}

pub fn estimate_tools(tools: &[ToolSpec]) -> u64 {
    tools
        .iter()
        .map(|tool| {
            estimate_tokens(&tool.name)
                + estimate_tokens(&tool.description)
                + estimate_tokens(&tool.parameters.to_string())
                + 8
        })
        .sum()
}

/// Conservative image estimate from dimensions (about 750 pixels per token).
pub fn estimate_image_tokens(width: u32, height: u32) -> u64 {
    let pixels = u64::from(width).saturating_mul(u64::from(height));

    pixels.div_ceil(750).saturating_add(85)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRange {
    pub start: usize,
    pub end: usize,
    pub current: bool,
}

/// Split conversation messages into turns beginning at each user message.
pub fn turn_ranges(messages: &[Message]) -> Vec<TurnRange> {
    let mut starts = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        if message.role == Role::User {
            starts.push(index);
        }
    }

    if starts.is_empty() {
        return Vec::new();
    }

    let last = starts.len() - 1;
    let end = messages.len();

    starts
        .iter()
        .enumerate()
        .map(|(index, start)| TurnRange {
            start: *start,
            end: if index + 1 < starts.len() {
                starts[index + 1]
            } else {
                end
            },
            current: index == last,
        })
        .collect()
}

/// Filesystem path helper used to identify session-owned images during estimates.
pub fn image_paths(messages: &[Message]) -> Vec<PathBuf> {
    messages
        .iter()
        .flat_map(Message::images)
        .map(|image| image.path.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        COMPACTION_KEEP_RECENT_TOKENS, COMPACTION_RESERVE_TOKENS, ContextBudget,
        DEFAULT_CONTEXT_LIMIT_FALLBACK, Projection, compaction_cut, estimate_message,
        estimate_messages, estimate_tools, resolve_output_budget, turn_ranges,
    };
    use crate::content::Content;
    use crate::message::Message;
    use crate::tool::{ToolCall, ToolSpec, ToolStatus};

    fn user(text: &str) -> Message {
        Message::user_text(text)
    }

    fn assistant(text: &str) -> Message {
        Message::assistant(vec![Content::text(text)])
    }

    fn tool_turn(call_id: &str) -> Vec<Message> {
        let call = ToolCall::new(call_id, "read_file", json!({ "path": "a" }));
        let result = crate::tool::ToolResult::new(&call, ToolStatus::Ok, "contents", false);

        vec![
            Message::assistant(vec![Content::ToolCall(call)]),
            Message::tool_results([result]),
        ]
    }

    #[test]
    fn two_complete_turns_and_the_current_turn_still_compact() {
        let mut messages = vec![user("one"), assistant("a")];
        messages.extend(tool_turn("call-1"));
        messages.extend([user("two"), assistant("b")]);
        messages.extend(tool_turn("call-2"));
        messages.extend([user("three"), assistant("c")]);
        messages.extend(tool_turn("call-3"));

        let cut = compaction_cut(&messages, 1).expect("cut");

        assert!(cut.cut > 0);
        assert!(cut.history_end <= cut.cut);
        assert!(
            messages[..cut.cut]
                .iter()
                .any(|message| message.role == crate::message::Role::User)
        );
    }

    #[test]
    fn a_cut_never_lands_on_a_tool_result() {
        let mut messages = vec![user(&"x".repeat(400))];

        messages.extend(tool_turn("call-1"));
        messages.push(assistant("done"));

        for keep in 1..20 {
            let cut = compaction_cut(&messages, keep);

            if let Some(cut) = cut {
                assert_ne!(messages[cut.cut].role, crate::message::Role::Tool);
            }
        }
    }

    #[test]
    fn a_split_turn_summarizes_its_prefix_and_keeps_the_rest() {
        let mut messages = vec![
            user(&"x".repeat(400)),
            assistant(&"a".repeat(400)),
            user(&"u".repeat(400)),
        ];

        messages.extend(tool_turn("call-1"));
        messages.push(assistant(&"b".repeat(400)));

        let cut = compaction_cut(&messages, 40).expect("cut");

        assert_eq!(messages[cut.cut].role, crate::message::Role::Assistant);
        assert!(cut.split_turn);
        assert!(cut.history_end < cut.cut);
        assert_eq!(messages[cut.history_end].role, crate::message::Role::User);
    }

    #[test]
    fn a_tiny_conversation_has_no_compactable_prefix() {
        let messages = vec![user("one"), assistant("a")];

        assert!(compaction_cut(&messages, COMPACTION_KEEP_RECENT_TOKENS).is_none());
    }

    #[test]
    fn the_compaction_trigger_uses_the_reserve_not_a_percentage() {
        let budget = ContextBudget::new(100_000);

        assert_eq!(budget.reserve_tokens, COMPACTION_RESERVE_TOKENS);
        assert_eq!(budget.keep_recent_tokens, COMPACTION_KEEP_RECENT_TOKENS);
        assert!(!budget.should_compact(100_000 - COMPACTION_RESERVE_TOKENS - 1));
        assert!(budget.should_compact(100_000 - COMPACTION_RESERVE_TOKENS));
    }

    #[test]
    fn a_fallback_window_scales_the_budgets_instead_of_disabling_compaction() {
        let budget = ContextBudget::new(DEFAULT_CONTEXT_LIMIT_FALLBACK);

        assert!(budget.can_compact(), "{budget:?}");
        assert!(budget.reserve_tokens < COMPACTION_RESERVE_TOKENS);
        assert!(budget.keep_recent_tokens < COMPACTION_KEEP_RECENT_TOKENS);
        assert!(
            budget.reserve_tokens + budget.keep_recent_tokens < budget.context_limit,
            "the scaled budgets must still fit: {budget:?}"
        );

        let trigger = DEFAULT_CONTEXT_LIMIT_FALLBACK - budget.reserve_tokens;

        assert!(budget.should_compact(trigger));
        assert!(!budget.should_compact(trigger - 1));

        // A window with no room at all still refuses rather than re-summarizing
        // the same messages forever.
        let degenerate = ContextBudget::new(1);

        assert!(!degenerate.can_compact());
        assert!(!degenerate.should_compact(u64::MAX));
    }

    /// A normal window must keep the nominal budgets: scaling them early would
    /// weaken the reserve on every ordinary session.
    #[test]
    fn a_normal_window_keeps_the_nominal_budgets() {
        let budget = ContextBudget::new(40_000);

        assert_eq!(budget.reserve_tokens, COMPACTION_RESERVE_TOKENS);
        assert_eq!(budget.keep_recent_tokens, COMPACTION_KEEP_RECENT_TOKENS);

        let boundary =
            ContextBudget::new(COMPACTION_RESERVE_TOKENS + COMPACTION_KEEP_RECENT_TOKENS);

        assert!(boundary.reserve_tokens < COMPACTION_RESERVE_TOKENS);
        assert!(boundary.can_compact(), "{boundary:?}");
    }

    /// The fallback window must reach a real cut, not only report that it can.
    #[test]
    fn a_fallback_window_conversation_yields_a_cut_with_a_bounded_suffix() {
        let budget = ContextBudget::new(DEFAULT_CONTEXT_LIMIT_FALLBACK);
        let mut messages = Vec::new();

        for index in 0..20 {
            messages.push(user(&format!("turn {index} {}", "x".repeat(4_000))));
            messages.push(assistant(&"a".repeat(4_000)));
        }

        let estimated = estimate_messages(&messages);

        assert!(
            estimated + budget.reserve_tokens >= budget.context_limit,
            "the fixture must overflow the fallback window: {estimated}"
        );
        assert!(budget.should_compact(estimated));

        let cut = budget.cut_point(&messages).expect("cut");

        assert!(cut.cut > 0 && cut.cut < messages.len());

        let retained = estimate_messages(&messages[cut.cut..]);

        // The boundary walk can move the cut back over tool messages, so the
        // suffix may overshoot its budget; it must still be far smaller than the
        // whole conversation, which is what makes the compaction worthwhile.
        assert!(
            retained <= budget.keep_recent_tokens + estimate_message(&messages[cut.cut]),
            "the retained suffix may overshoot by at most one message: {retained}"
        );
        assert!(
            retained < estimated,
            "the cut must keep less than it summarizes: {retained} of {estimated}"
        );
    }

    #[test]
    fn output_budget_defaults_to_twenty_percent_and_respects_the_cap() {
        assert_eq!(resolve_output_budget(None, 1000, None).unwrap(), 200);
        assert_eq!(resolve_output_budget(None, 1000, Some(100)).unwrap(), 100);
        assert_eq!(
            resolve_output_budget(Some(50), 1000, Some(100)).unwrap(),
            50
        );
        assert!(resolve_output_budget(Some(101), 1000, Some(100)).is_err());
        assert!(resolve_output_budget(Some(1001), 1000, None).is_err());
    }

    #[test]
    fn projection_only_counts_uncovered_messages() {
        let messages = vec![user(&"x".repeat(400)), user("recent")];
        let projection = Projection {
            summary: "short".to_owned(),
            covered_messages: 1,
        };

        assert!(
            super::estimate_projected(&messages, Some(&projection), &[], 0)
                < super::estimate_projected(&messages, None, &[], 0)
        );
    }

    #[test]
    fn turn_ranges_start_at_user_messages() {
        let messages = vec![user("one"), assistant("a"), user("two")];
        let turns = turn_ranges(&messages);

        assert_eq!(turns.len(), 2);
        assert_eq!((turns[0].start, turns[0].end), (0, 2));
        assert_eq!((turns[1].start, turns[1].end), (2, 3));
        assert!(turns[1].current);
    }

    #[test]
    fn tool_schema_estimates_are_positive() {
        let spec = ToolSpec {
            name: "bash".to_owned(),
            description: "run".to_owned(),
            parameters: json!({ "type": "object" }),
        };

        assert!(estimate_tools(&[spec]) > 0);
        assert!(estimate_message(&assistant("hello")) > 0);
    }
}
