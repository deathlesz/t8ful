use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

use crate::fsutil::{read_text, write_atomically};
use crate::paths;

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    edits: Vec<EditEntry>,
}

#[derive(Debug, Deserialize)]
struct EditEntry {
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Debug)]
struct Replacement {
    /// Index of the edit that produced this replacement, for error messages.
    edit: usize,
    start: usize,
    end: usize,
    new_text: String,
}

#[derive(Debug, Default)]
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".to_owned(),
            description: "Make one or more exact-text replacements in one file in a single \
                          atomic write. Every edit is matched against the original text; each \
                          old_text must match exactly once unless its own replace_all is set. \
                          Overlapping edits fail and nothing is written."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, absolute or relative to the launch directory" },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Replacements, all located in the original file",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": { "type": "string", "description": "Exact text to replace, including indentation" },
                                "new_text": { "type": "string", "description": "Replacement text" },
                                "replace_all": { "type": "boolean", "description": "Replace every occurrence of this old_text (default false)" }
                            },
                            "required": ["old_text", "new_text"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: EditArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;

        if args.edits.is_empty() {
            return Err(ToolError::InvalidArguments(
                "edits must contain at least one edit".to_owned(),
            ));
        }

        let path = paths::resolve(&ctx.launch_dir, &args.path);

        crate::fsutil::reject_leaf_symlink(&path).map_err(ToolError::Failed)?;

        let original = read_text(&path).map_err(ToolError::Failed)?;
        let mut replacements = Vec::new();

        for (index, edit) in args.edits.iter().enumerate() {
            if edit.old_text.is_empty() {
                return Err(ToolError::InvalidArguments(format!(
                    "edits[{index}]: old_text must not be empty; use write_file to create a file"
                )));
            }

            let matches = match_positions(&original, &edit.old_text);

            if matches.is_empty() {
                return Err(ToolError::Failed(format!(
                    "edits[{index}]: no exact match for old_text in {}",
                    paths::display(&path, &ctx.project_root)
                )));
            }

            if matches.len() > 1 && !edit.replace_all {
                let lines = matches
                    .iter()
                    .map(|index| line_number(&original, *index))
                    .collect::<Vec<_>>();

                return Err(ToolError::Failed(format!(
                    "edits[{index}]: old_text matches {} times in {} (lines {}); add more \
                     context or set replace_all",
                    matches.len(),
                    paths::display(&path, &ctx.project_root),
                    lines
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }

            if edit.replace_all && overlaps(&matches, edit.old_text.len()) {
                return Err(ToolError::Failed(format!(
                    "edits[{index}]: old_text has overlapping matches in {}; replace_all would \
                     have ambiguous semantics, so nothing was written. Use a longer, \
                     non-overlapping match",
                    paths::display(&path, &ctx.project_root)
                )));
            }

            let starts = if edit.replace_all {
                matches.as_slice()
            } else {
                &matches[..1]
            };

            for start in starts {
                replacements.push(Replacement {
                    edit: index,
                    start: *start,
                    end: start.saturating_add(edit.old_text.len()),
                    new_text: edit.new_text.clone(),
                });
            }
        }

        replacements.sort_by_key(|replacement| replacement.start);

        for pair in replacements.windows(2) {
            if pair[1].start < pair[0].end {
                return Err(ToolError::Failed(format!(
                    "edits[{}]: the replacement range overlaps edits[{}]; nothing was written",
                    pair[1].edit, pair[0].edit
                )));
            }
        }

        let replacement_count = replacements.len();
        let mut updated = String::with_capacity(original.len());
        let mut cursor = 0;

        for replacement in &replacements {
            updated.push_str(&original[cursor..replacement.start]);
            updated.push_str(&replacement.new_text);
            cursor = replacement.end;
        }

        updated.push_str(&original[cursor..]);

        write_atomically(&path, &updated, false)
            .map_err(|error| ToolError::Failed(error.to_string()))?;

        let edit_count = args.edits.len();

        Ok(ToolOutput {
            content: format!(
                "applied {edit_count} edit{} ({replacement_count} replacement{}) in {} \
                 ({} bytes)",
                if edit_count == 1 { "" } else { "s" },
                if replacement_count == 1 { "" } else { "s" },
                paths::display(&path, &ctx.project_root),
                updated.len()
            ),
            truncated: false,
            metadata: Some(json!({
                "path": path.display().to_string(),
                "edits": edit_count,
                "replacements": replacement_count,
                "bytes": updated.len(),
            })),
        })
    }
}

/// Every start position where `needle` matches, including overlapping matches.
///
/// `str::match_indices` only reports non-overlapping matches, which would hide
/// an ambiguous `old_text` such as `aa` in `aaa`.
fn match_positions(haystack: &str, needle: &str) -> Vec<usize> {
    haystack
        .char_indices()
        .map(|(index, _)| index)
        .filter(|index| haystack[*index..].starts_with(needle))
        .collect()
}

fn overlaps(matches: &[usize], length: usize) -> bool {
    matches
        .windows(2)
        .any(|pair| pair[1] < pair[0].saturating_add(length))
}

fn line_number(text: &str, index: usize) -> usize {
    text[..index].bytes().filter(|byte| *byte == b'\n').count() + 1
}
