use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

use crate::fsutil::write_atomically;
use crate::paths;

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
    #[serde(default)]
    create_parents: bool,
}

#[derive(Debug, Default)]
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".to_owned(),
            description: "Create a file or deliberately replace its entire contents. Use \
                          edit_file for small changes to an existing file."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, absolute or relative to the launch directory" },
                    "content": { "type": "string", "description": "Complete new file contents" },
                    "create_parents": { "type": "boolean", "description": "Create missing parent directories (default false)" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: WriteArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;

        let path = paths::resolve(&ctx.launch_dir, &args.path);

        if path.is_dir() {
            return Err(ToolError::Failed(format!(
                "{} is a directory",
                path.display()
            )));
        }

        let existed = path.exists();

        write_atomically(&path, &args.content, args.create_parents)
            .map_err(|error| ToolError::Failed(error.to_string()))?;

        let bytes = args.content.len();
        let action = if existed { "replaced" } else { "created" };

        Ok(ToolOutput {
            content: format!(
                "{action} {} ({bytes} bytes)",
                paths::display(&path, &ctx.project_root)
            ),
            truncated: false,
            metadata: Some(json!({
                "path": path.display().to_string(),
                "bytes": bytes,
                "created": !existed,
            })),
        })
    }
}
