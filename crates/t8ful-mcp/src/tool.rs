//! The core [`Tool`] adapter around one MCP server tool.

use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject, Tool as McpToolDef};
use serde_json::{Map, Value};
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

use crate::client::ClientHandle;
use crate::config::ServerConfig;
use crate::render;

/// One model-visible tool backed by an MCP server.
///
/// Cloning is cheap and shares nothing mutable: the hub clones the adapter into
/// one `Arc` per call to [`McpHub::tools`], and every clone talks to the same
/// server through the same [`ClientHandle`].
#[derive(Clone)]
pub struct McpTool {
    /// The model-visible name, `mcp__server__tool`.
    name: String,
    /// The server's own spelling, which `tools/call` must send.
    remote_name: String,
    server: String,
    spec: ToolSpec,
    plan: crate::config::PlanTrust,
    client: ClientHandle,
}

impl McpTool {
    pub fn new(
        model_name: String,
        server: &ServerConfig,
        definition: &McpToolDef,
        client: ClientHandle,
    ) -> Self {
        let remote_name = definition.name.to_string();
        let parameters = schema_of(definition);
        let description = description_of(server, definition);
        let spec = ToolSpec {
            name: model_name.clone(),
            description,
            parameters,
        };

        Self {
            name: model_name,
            remote_name,
            server: server.name.clone(),
            spec,
            plan: server.plan,
            client,
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    /// The model-visible name, `mcp__server__tool`.
    pub fn model_name(&self) -> &str {
        &self.name
    }

    /// The server's own tool name, as `tools/call` sends it.
    pub fn remote_name(&self) -> &str {
        &self.remote_name
    }
}

/// The server's `inputSchema` when it is a JSON object, else an empty object
/// schema.
///
/// A malformed schema is the server's problem and its own error is surfaced at
/// call time; the model must still be offered the tool, so the fallback is a
/// permissive object rather than dropping it.
fn schema_of(definition: &McpToolDef) -> Value {
    Value::Object((*definition.input_schema).clone())
}

fn description_of(server: &ServerConfig, definition: &McpToolDef) -> String {
    let base = definition
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "`{}` from the `{}` MCP server",
                definition.name, server.name
            )
        });

    if server.plan.is_read_only() {
        base
    } else {
        format!(
            "{base} (MCP server `{}`; available in coding mode only unless it is trusted for \
             planning)",
            server.name
        )
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn effect(&self) -> ToolEffect {
        if self.plan.is_read_only() {
            ToolEffect::ReadOnly
        } else {
            ToolEffect::Mutating
        }
    }

    fn denial_reason(&self) -> Option<String> {
        if self.plan.is_read_only() {
            return None;
        }

        Some(crate::hub::untrusted_message(&self.server))
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let arguments = arguments_object(&call.arguments)?;
        let client = self.client.current().ok_or_else(|| {
            ToolError::Failed(format!(
                "MCP server `{}` is not running, so `{}` cannot be called",
                self.server, self.remote_name
            ))
        })?;

        let request =
            CallToolRequestParams::new(self.remote_name.clone()).with_arguments(arguments);

        let result = client.call_tool(request).await.map_err(|error| {
            ToolError::Failed(format!(
                "MCP server `{}` could not run `{}`: {error}",
                self.server, self.remote_name
            ))
        })?;

        finish(result, ctx.max_output_bytes)
    }
}

/// MCP arguments must be a JSON object; deeper validation is the server's job.
fn arguments_object(value: &Value) -> Result<JsonObject, ToolError> {
    match value {
        Value::Object(map) => Ok(map.clone()),
        Value::Null => Ok(Map::new()),
        other => Err(ToolError::InvalidArguments(format!(
            "MCP tool arguments must be a JSON object, not {}",
            kind_of(other)
        ))),
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Bound a server result the way `bash` bounds its output.
fn finish(result: CallToolResult, limit: usize) -> Result<ToolOutput, ToolError> {
    let (text, is_error) = render::render(&result);
    let (text, truncated) = crate::text::truncate(text, limit);

    if is_error {
        return Err(ToolError::Failed(text));
    }

    Ok(ToolOutput::text(text).with_truncation(truncated))
}

#[cfg(test)]
mod tests {
    use super::{arguments_object, finish};
    use rmcp::model::{CallToolResult, ContentBlock};
    use serde_json::json;

    fn text_result(body: &str) -> CallToolResult {
        CallToolResult::success(vec![ContentBlock::text(body)])
    }

    #[test]
    fn a_non_object_argument_is_rejected() {
        assert!(arguments_object(&json!({"a": 1})).is_ok());
        assert!(arguments_object(&json!(null)).is_ok());

        for value in [json!(1), json!("text"), json!([1, 2]), json!(true)] {
            assert!(arguments_object(&value).is_err(), "{value}");
        }
    }

    #[test]
    fn an_error_result_becomes_a_failure() {
        assert!(
            finish(
                CallToolResult::error(vec![ContentBlock::text("boom")]),
                1024
            )
            .is_err()
        );
    }

    #[test]
    fn an_oversized_result_is_truncated_and_says_so() {
        let output = finish(text_result(&"a".repeat(4096)), 256).expect("not an error");

        assert!(output.truncated);
        assert!(output.content.len() <= 256, "{}", output.content.len());
    }

    #[test]
    fn a_result_within_the_limit_is_not_flagged() {
        let output = finish(text_result("short"), 256).expect("not an error");

        assert!(!output.truncated);
        assert_eq!(output.content, "short");
    }
}
