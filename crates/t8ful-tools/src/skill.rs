//! The `skill` tool and the library interface skill discovery implements.
//!
//! Loading a skill is read-only: it returns instructions and a list of
//! supporting files. Nothing in a skill repository is executed here, and a
//! skill cannot widen the policy of the current mode.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

/// The short, stable view of a skill exposed in the system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

/// A skill's instructions plus its supporting files, loaded on demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDocument {
    pub name: String,
    pub directory: PathBuf,
    pub body: String,
    /// Paths relative to `directory`, sorted.
    pub files: Vec<PathBuf>,
    /// The supporting-file listing was capped.
    pub truncated: bool,
}

/// Discovery backend used by [`SkillTool`] and explicit activation.
pub trait SkillLibrary: Send + Sync {
    fn summaries(&self) -> Vec<SkillSummary>;

    fn load(&self, name: &str) -> Result<SkillDocument, String>;
}

/// A library with no roots to scan.
#[derive(Debug, Default)]
pub struct NoSkills;

impl SkillLibrary for NoSkills {
    fn summaries(&self) -> Vec<SkillSummary> {
        Vec::new()
    }

    fn load(&self, name: &str) -> Result<SkillDocument, String> {
        Err(format!("unknown skill `{name}`: no skills are installed"))
    }
}

/// Load an installed skill's instructions by name.
pub struct SkillTool {
    library: Arc<dyn SkillLibrary>,
}

impl std::fmt::Debug for SkillTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillTool").finish_non_exhaustive()
    }
}

impl SkillTool {
    pub fn new(library: Arc<dyn SkillLibrary>) -> Self {
        Self { library }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillArgs {
    name: String,
}

#[async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill".to_owned(),
            description: "Load an installed skill's instructions by name. The <skills> section \
                          of the system prompt lists the available names; when that section is \
                          absent, no skills are installed."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Skill directory name from the <skills> section"
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: SkillArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let name = args.name.trim();

        if name.is_empty() {
            return Err(ToolError::InvalidArguments(
                "the skill name must not be empty".to_owned(),
            ));
        }

        let document = self
            .library
            .load(name)
            .map_err(ToolError::InvalidArguments)?;
        let (content, truncated) = format_skill(&document, ctx.max_output_bytes);

        Ok(ToolOutput::text(content).with_truncation(truncated))
    }
}

/// Render a loaded skill for the model, truncating safely at `max_bytes`.
///
/// The same wrapper is used when the model asks for a skill and when the user
/// activates one explicitly, so both paths look identical in the transcript.
pub fn format_skill(document: &SkillDocument, max_bytes: usize) -> (String, bool) {
    let mut text = format!(
        "<skill name=\"{}\" root=\"{}\">\n\
         The user loaded this skill explicitly; follow it where it applies.\n\
         {}\n\
         </skill>",
        escape_attribute(&document.name),
        escape_attribute(&document.directory.display().to_string()),
        document.body
    );

    if !document.files.is_empty() {
        text.push_str(
            "\n\nSupporting files (read with read_file; run scripts with bash under the policy \
             in effect):",
        );

        for file in &document.files {
            text.push_str("\n- ");
            text.push_str(&file.display().to_string());
        }
    }

    if document.truncated {
        text.push_str("\n(the supporting-file listing was truncated)");
    }

    crate::text::truncate(text, max_bytes)
}

fn escape_attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            other => escaped.push(other),
        }
    }

    escaped
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect};

    use super::{NoSkills, SkillDocument, SkillLibrary, SkillTool, format_skill};

    struct Stub {
        document: SkillDocument,
    }

    impl SkillLibrary for Stub {
        fn summaries(&self) -> Vec<super::SkillSummary> {
            vec![super::SkillSummary {
                name: self.document.name.clone(),
                description: "Review a change.".to_owned(),
            }]
        }

        fn load(&self, name: &str) -> Result<SkillDocument, String> {
            if name == self.document.name {
                Ok(self.document.clone())
            } else {
                Err(format!("unknown skill `{name}`; available: review"))
            }
        }
    }

    fn document() -> SkillDocument {
        SkillDocument {
            name: "review".to_owned(),
            directory: PathBuf::from("/home/u/.config/t8ful/skills/review"),
            body: "Check the change.".to_owned(),
            files: vec![PathBuf::from("references/checklist.md")],
            truncated: false,
        }
    }

    fn context() -> ToolContext {
        let mut context = ToolContext::new(PathBuf::from("/work"), PathBuf::from("/work"));

        context.max_output_bytes = 4096;
        context
    }

    fn call(name: &str) -> ToolCall {
        ToolCall::new("call-1", "skill", serde_json::json!({ "name": name }))
    }

    #[test]
    fn the_tool_is_read_only_and_rejects_extra_arguments() {
        let tool = SkillTool::new(std::sync::Arc::new(NoSkills));

        assert_eq!(tool.effect(), ToolEffect::ReadOnly);
        assert!(matches!(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime")
                .block_on(tool.execute(
                    &ToolCall::new(
                        "call-1",
                        "skill",
                        serde_json::json!({ "name": "review", "extra": true })
                    ),
                    &context()
                )),
            Err(t8ful_core::tool::ToolError::InvalidArguments(_))
        ));
    }

    #[tokio::test]
    async fn the_tool_formats_a_loaded_skill() {
        let tool = SkillTool::new(std::sync::Arc::new(Stub {
            document: document(),
        }));
        let output = tool
            .execute(&call("review"), &context())
            .await
            .expect("load");

        assert!(
            output
                .content
                .contains("<skill name=\"review\" root=\"/home/u/.config/t8ful/skills/review\">")
        );
        assert!(
            output
                .content
                .contains("The user loaded this skill explicitly; follow it where it applies.")
        );
        assert!(output.content.contains("Check the change."));
        assert!(
            output.content.contains("- references/checklist.md"),
            "{}",
            output.content
        );
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn an_unknown_name_lists_the_available_skills() {
        let tool = SkillTool::new(std::sync::Arc::new(Stub {
            document: document(),
        }));
        let error = tool
            .execute(&call("missing"), &context())
            .await
            .expect_err("unknown");

        assert!(error.to_string().contains("review"), "{error}");

        let empty = SkillTool::new(std::sync::Arc::new(NoSkills));
        let error = empty
            .execute(&call("review"), &context())
            .await
            .expect_err("no skills");

        assert!(error.to_string().contains("no skills"), "{error}");
    }

    #[test]
    fn formatting_truncates_on_a_character_boundary_and_reports_it() {
        let document = SkillDocument {
            name: "review".to_owned(),
            directory: PathBuf::from("/skills/review"),
            body: "é".repeat(100),
            files: Vec::new(),
            truncated: false,
        };
        let (text, truncated) = format_skill(&document, 120);

        assert!(truncated);
        assert!(text.len() <= 120, "{}", text.len());
        assert!(text.is_char_boundary(text.len()));

        let (text, truncated) = format_skill(&document, 4096);

        assert!(!truncated);
        assert!(text.contains(&"é".repeat(100)));

        let (text, truncated) = format_skill(&document, 5);

        assert!(truncated);
        assert!(text.len() <= 5, "{text:?}");
    }

    #[test]
    fn attribute_values_are_escaped() {
        let document = SkillDocument {
            name: "a\"b".to_owned(),
            directory: PathBuf::from("/skills/a&b"),
            body: String::new(),
            files: Vec::new(),
            truncated: false,
        };
        let (text, _) = format_skill(&document, 4096);

        assert!(text.contains("name=\"a&quot;b\""), "{text}");
        assert!(text.contains("root=\"/skills/a&amp;b\""), "{text}");
    }
}
