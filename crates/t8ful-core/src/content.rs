use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolResult};

/// A validated image stored in session-owned storage.
///
/// The bytes live outside the conversation so resuming a session does not
/// require duplicating a large base64 payload in the journal. Providers read the
/// file only when building a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInput {
    pub media_type: String,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

impl ImageInput {
    pub fn new(
        media_type: impl Into<String>,
        path: PathBuf,
        width: u32,
        height: u32,
        bytes: u64,
    ) -> Self {
        Self {
            media_type: media_type.into(),
            path,
            width,
            height,
            bytes,
        }
    }
}

/// One piece of normalized conversation content.
///
/// Providers translate these variants to their wire format; nothing in the core
/// assumes a particular protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Content {
    Text { text: String },
    Image(ImageInput),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image(image: ImageInput) -> Self {
        Self::Image(image)
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    pub fn as_image(&self) -> Option<&ImageInput> {
        match self {
            Self::Image(image) => Some(image),
            _ => None,
        }
    }

    pub fn as_tool_call(&self) -> Option<&ToolCall> {
        match self {
            Self::ToolCall(call) => Some(call),
            _ => None,
        }
    }

    pub fn as_tool_result(&self) -> Option<&ToolResult> {
        match self {
            Self::ToolResult(result) => Some(result),
            _ => None,
        }
    }
}
