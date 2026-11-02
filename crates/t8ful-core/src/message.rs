use serde::{Deserialize, Serialize};

use crate::content::{Content, ImageInput};
use crate::tool::{ToolCall, ToolResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

impl Message {
    pub fn new(role: Role, content: Vec<Content>) -> Self {
        Self { role, content }
    }

    pub fn system_text(text: impl Into<String>) -> Self {
        Self::new(Role::System, vec![Content::text(text)])
    }

    pub fn user_text(text: impl Into<String>) -> Self {
        Self::new(Role::User, vec![Content::text(text)])
    }

    pub fn user_with_images(text: Option<String>, images: Vec<ImageInput>) -> Self {
        let mut content = Vec::new();

        if let Some(text) = text
            && !text.is_empty()
        {
            content.push(Content::text(text));
        }

        content.extend(images.into_iter().map(Content::image));

        Self::new(Role::User, content)
    }

    pub fn assistant(content: Vec<Content>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn tool_results(results: impl IntoIterator<Item = ToolResult>) -> Self {
        let content = results
            .into_iter()
            .map(Content::ToolResult)
            .collect::<Vec<_>>();

        Self::new(Role::Tool, content)
    }

    /// Concatenated text content, ignoring tool calls and results.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(Content::as_text)
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn images(&self) -> impl Iterator<Item = &ImageInput> {
        self.content.iter().filter_map(Content::as_image)
    }

    pub fn has_images(&self) -> bool {
        self.content
            .iter()
            .any(|content| content.as_image().is_some())
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(Content::as_tool_call)
    }

    pub fn results(&self) -> impl Iterator<Item = &ToolResult> {
        self.content.iter().filter_map(Content::as_tool_result)
    }
}

/// Append-only conversation history for the current process.
///
/// Phase 1 keeps this in memory only; the versioned on-disk session format
/// arrives in Phase 2 and reuses this type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    pub fn push_user_text(&mut self, text: impl Into<String>) {
        self.push(Message::user_text(text));
    }

    pub fn last(&self) -> Option<&Message> {
        self.messages.last()
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Whether a tool result for `call_id` is already recorded.
    ///
    /// The agent consults this before dispatch so a restarted or replayed turn
    /// never executes an action twice.
    pub fn has_result_for(&self, call_id: &str) -> bool {
        self.messages
            .iter()
            .flat_map(Message::results)
            .any(|result| result.call_id.as_str() == call_id)
    }

    /// Tool calls in the history that have no recorded result yet.
    pub fn unanswered_tool_calls(&self) -> Vec<&ToolCall> {
        let answered = self
            .messages
            .iter()
            .flat_map(Message::results)
            .map(|result| result.call_id.as_str())
            .collect::<Vec<_>>();

        self.messages
            .iter()
            .flat_map(Message::tool_calls)
            .filter(|call| !answered.contains(&call.id.as_str()))
            .collect()
    }
}
