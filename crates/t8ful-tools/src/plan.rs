use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolSpec};

#[derive(Debug, Deserialize)]
struct PlanArgs {
    plan: String,
}

#[derive(Debug, Clone, Copy)]
pub struct PresentPlanTool;

impl Default for PresentPlanTool {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for PresentPlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "present_plan".to_owned(),
            description: "Present a complete implementation plan to the user and return control. \
                          This does not switch to coding mode; the user decides whether to leave \
                          plan mode."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "string",
                        "description": "Complete Markdown plan"
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Control
    }

    async fn execute(
        &self,
        call: &ToolCall,
        _context: &ToolContext,
    ) -> Result<t8ful_core::tool::ToolOutput, t8ful_core::tool::ToolError> {
        let args: PlanArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| t8ful_core::ToolError::InvalidArguments(error.to_string()))?;

        if args.plan.trim().is_empty() {
            return Err(t8ful_core::ToolError::InvalidArguments(
                "the plan must not be empty".to_owned(),
            ));
        }

        Ok(t8ful_core::ToolOutput {
            content: "the plan was stored for review. This call did not change the mode and did \
not approve the plan; the user's next message is the instruction."
                .to_owned(),
            truncated: false,
            metadata: Some(json!({ "present_plan": true })),
            images: Vec::new(),
        })
    }
}
