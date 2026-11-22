//! Prompt serialization for the OpenAI chat-completions dialect.

use base64::Engine;
use serde_json::{Value, json};
use t8ful_core::error::ProviderError;
use t8ful_core::message::{Message, Role};
use t8ful_core::provider::{ProviderRequest, ReasoningEffort};

use crate::profile::{ProtocolProfile, ReasoningStyle, UsageStyle};

/// Render a request as the JSON body both providers POST.
///
/// Everything provider-specific comes from `profile`, so the two providers
/// cannot drift in how a conversation, a tool spec, or a reasoning level is
/// spelled.
pub fn request_body(
    request: &ProviderRequest,
    profile: &ProtocolProfile,
) -> Result<Value, ProviderError> {
    let mut messages = Vec::new();
    // Tool results arrive as one `role: "tool"` message each, and a strict
    // provider requires every one of them to follow the assistant message that
    // declared its call. Chat Completions also accepts an image only in a user
    // or assistant message, so an image a tool returned is held here and emitted
    // as a single user message once the whole batch is closed.
    let mut pending_images: Vec<t8ful_core::content::ImageInput> = Vec::new();

    if !request.system.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": request.system }));
    }

    for message in &request.messages {
        if message.role != Role::Tool && !pending_images.is_empty() {
            messages.push(image_message(std::mem::take(&mut pending_images))?);
        }

        for value in message_values(message)? {
            messages.push(value);
        }

        pending_images.extend(
            message
                .results()
                .flat_map(|result| result.images.iter().cloned()),
        );
    }

    if !pending_images.is_empty() {
        messages.push(image_message(pending_images)?);
    }

    let mut body = json!({
        "model": request.model,
        "stream": true,
        "messages": messages,
    });

    match profile.usage_style {
        UsageStyle::Body => body["usage"] = json!({ "include": true }),
        UsageStyle::StreamOptions => body["stream_options"] = json!({ "include_usage": true }),
    }

    if !request.tools.is_empty() {
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                })
            })
            .collect::<Vec<_>>();

        body["tools"] = json!(tools);
    }

    if let Some(max_tokens) = request.max_output_tokens {
        body["max_tokens"] = json!(max_tokens);
    }

    match profile.reasoning_style {
        ReasoningStyle::Object => match request.reasoning {
            ReasoningEffort::Default => {}
            ReasoningEffort::Off => body["reasoning"] = json!({ "enabled": false }),
            effort => {
                let value = request.reasoning_value.as_deref().unwrap_or(effort.label());

                body["reasoning"] = json!({ "effort": value });
            }
        },
        ReasoningStyle::Flat => match request.reasoning {
            // The flat spelling has no "off": leaving the field out is the only
            // way to ask those endpoints for no reasoning.
            ReasoningEffort::Default | ReasoningEffort::Off => {}
            effort => {
                let value = request.reasoning_value.as_deref().unwrap_or(effort.label());

                body["reasoning_effort"] = json!(value);
            }
        },
    }

    Ok(body)
}

pub fn message_values(message: &Message) -> Result<Vec<Value>, ProviderError> {
    Ok(match message.role {
        Role::System => vec![json!({ "role": "system", "content": message.text() })],
        Role::User => {
            if message.has_images() {
                let mut parts = Vec::new();
                let text = message.text();

                if !text.is_empty() {
                    parts.push(json!({ "type": "text", "text": text }));
                }

                for image in message.images() {
                    parts.push(image_part(image)?);
                }

                vec![json!({ "role": "user", "content": parts })]
            } else {
                vec![json!({ "role": "user", "content": message.text() })]
            }
        }
        Role::Assistant => {
            let text = message.text();
            let calls = message
                .tool_calls()
                .map(|call| {
                    json!({
                        "id": call.id.as_str(),
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments.to_string(),
                        }
                    })
                })
                .collect::<Vec<_>>();

            let content = if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            };
            let mut value = json!({ "role": "assistant", "content": content });

            if !calls.is_empty() {
                value["tool_calls"] = json!(calls);
            }

            vec![value]
        }
        Role::Tool => message
            .results()
            .map(|result| {
                json!({
                    "role": "tool",
                    "tool_call_id": result.call_id.as_str(),
                    "content": result.content,
                })
            })
            .collect(),
    })
}

/// One user message carrying the images a tool batch returned.
///
/// The caption lives in the tool result's own text, so this message is only the
/// pixels: a blank text part would be noise, and providers accept a content list
/// of image parts alone.
fn image_message(images: Vec<t8ful_core::content::ImageInput>) -> Result<Value, ProviderError> {
    let parts = images
        .iter()
        .map(image_part)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(json!({ "role": "user", "content": parts }))
}

/// Read one stored attachment and render it as an `image_url` part.
fn image_part(image: &t8ful_core::content::ImageInput) -> Result<Value, ProviderError> {
    let bytes = std::fs::read(&image.path).map_err(|error| {
        ProviderError::Other(format!(
            "the image attachment {} is missing or unreadable: {error}; resume the session with \
             its attachment directory intact or remove the attachment",
            image.path.display()
        ))
    })?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);

    Ok(json!({
        "type": "image_url",
        "image_url": {
            "url": format!("data:{};base64,{encoded}", image.media_type)
        }
    }))
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use serde_json::json;
    use t8ful_core::message::{Conversation, Message};
    use t8ful_core::provider::{ProviderRequest, ReasoningEffort};
    use t8ful_core::tool::{ToolCall, ToolResult, ToolSpec, ToolStatus};

    use super::{message_values, request_body};
    use crate::profile::ProtocolProfile;

    fn request() -> ProviderRequest {
        ProviderRequest {
            model: "openrouter/test".to_owned(),
            system: String::new(),
            messages: vec![Message::user_text("go")],
            tools: Vec::new(),
            max_output_tokens: None,
            reasoning: ReasoningEffort::Default,
            reasoning_value: None,
            session_id: None,
        }
    }

    #[test]
    fn assistant_tool_calls_serialize_as_function_calls() {
        let message = Message::assistant(vec![
            t8ful_core::Content::text("thinking"),
            t8ful_core::Content::ToolCall(ToolCall::new(
                "call-1",
                "read_file",
                json!({ "path": "a.txt" }),
            )),
        ]);

        let values = message_values(&message).unwrap();

        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["role"], json!("assistant"));
        assert_eq!(values[0]["content"], json!("thinking"));
        assert_eq!(values[0]["tool_calls"][0]["id"], json!("call-1"));
        assert_eq!(
            values[0]["tool_calls"][0]["function"]["arguments"],
            json!("{\"path\":\"a.txt\"}")
        );
    }

    #[test]
    fn tool_results_become_one_message_each() {
        let call = ToolCall::new("call-1", "read_file", json!({}));
        let message = Message::tool_results([
            ToolResult::new(&call, ToolStatus::Ok, "first", false),
            ToolResult::new(&call, ToolStatus::Denied, "second", false),
        ]);

        let values = message_values(&message).unwrap();

        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["tool_call_id"], json!("call-1"));
        assert_eq!(values[1]["content"], json!("second"));
    }

    #[test]
    fn request_body_includes_tools_only_when_present() {
        let mut conversation = Conversation::new();

        conversation.push_user_text("hello");

        let mut prompt = request();

        prompt.system = "be brief".to_owned();
        prompt.messages = conversation.messages().to_vec();

        let body = request_body(&prompt, &ProtocolProfile::OPENROUTER).unwrap();

        assert!(body.get("tools").is_none());
        assert!(body.get("reasoning").is_none());
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["model"], json!("openrouter/test"));
        assert_eq!(body["messages"][0]["role"], json!("system"));
        assert_eq!(body["messages"][0]["content"], json!("be brief"));
        assert_eq!(body["messages"][1]["role"], json!("user"));
        // The OpenRouter body keeps its original shape byte for byte.
        assert_eq!(body["usage"], json!({ "include": true }));
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn request_body_serializes_tool_specs() {
        let mut prompt = request();

        prompt.tools = vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "reads".to_owned(),
            parameters: json!({ "type": "object" }),
        }];
        prompt.max_output_tokens = Some(64);

        let body = request_body(&prompt, &ProtocolProfile::OPENROUTER).unwrap();

        assert_eq!(body["tools"][0]["function"]["name"], json!("read_file"));
        assert_eq!(body["max_tokens"], json!(64));
    }

    #[test]
    fn the_object_reasoning_style_sends_the_openrouter_field() {
        let mut prompt = request();

        assert!(
            request_body(&prompt, &ProtocolProfile::OPENROUTER)
                .unwrap()
                .get("reasoning")
                .is_none()
        );

        prompt.reasoning = ReasoningEffort::Off;
        assert_eq!(
            request_body(&prompt, &ProtocolProfile::OPENROUTER).unwrap()["reasoning"],
            json!({ "enabled": false })
        );

        for (effort, expected) in [
            (ReasoningEffort::Minimal, "minimal"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::XHigh, "xhigh"),
            (ReasoningEffort::Max, "max"),
        ] {
            prompt.reasoning = effort;
            assert_eq!(
                request_body(&prompt, &ProtocolProfile::OPENROUTER).unwrap()["reasoning"],
                json!({ "effort": expected })
            );
        }
    }

    #[test]
    fn a_mapped_level_sends_the_provider_value() {
        let mut prompt = request();

        prompt.reasoning = ReasoningEffort::High;
        prompt.reasoning_value = Some("adaptive".to_owned());

        assert_eq!(
            request_body(&prompt, &ProtocolProfile::OPENROUTER).unwrap()["reasoning"],
            json!({ "effort": "adaptive" })
        );

        prompt.reasoning = ReasoningEffort::Off;
        prompt.reasoning_value = Some("adaptive".to_owned());

        assert_eq!(
            request_body(&prompt, &ProtocolProfile::OPENROUTER).unwrap()["reasoning"],
            json!({ "enabled": false })
        );
    }

    #[test]
    fn the_flat_reasoning_style_sends_reasoning_effort_and_no_object() {
        let mut prompt = request();
        let go = ProtocolProfile::OPENCODE_GO;

        let body = request_body(&prompt, &go).unwrap();

        assert!(body.get("reasoning").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["stream_options"], json!({ "include_usage": true }));
        assert!(body.get("usage").is_none());

        prompt.reasoning = ReasoningEffort::Off;
        assert!(
            request_body(&prompt, &go)
                .unwrap()
                .get("reasoning_effort")
                .is_none()
        );

        prompt.reasoning = ReasoningEffort::High;
        assert_eq!(
            request_body(&prompt, &go).unwrap()["reasoning_effort"],
            json!("high")
        );

        prompt.reasoning_value = Some("max".to_owned());
        assert_eq!(
            request_body(&prompt, &go).unwrap()["reasoning_effort"],
            json!("max")
        );
    }

    #[test]
    fn a_missing_attachment_is_reported() {
        let message = Message::user_with_images(
            Some("attachment".to_owned()),
            vec![t8ful_core::content::ImageInput::new(
                "image/png",
                std::path::PathBuf::from("/definitely/not/a/real-t8ful-image.png"),
                1,
                1,
                8,
            )],
        );

        let error = message_values(&message).expect_err("missing attachment");

        assert!(
            error.to_string().contains("missing or unreadable"),
            "{error}"
        );
    }

    /// Write real bytes to a temporary file so the base64 path is exercised.
    fn stored_image(dir: &tempfile::TempDir, name: &str) -> t8ful_core::content::ImageInput {
        let path = dir.path().join(name);

        std::fs::write(&path, b"pixels").unwrap();

        t8ful_core::content::ImageInput::new("image/png", path, 1200, 800, 6)
    }

    #[test]
    fn a_tool_result_never_carries_an_image_url() {
        // Chat Completions accepts `image_url` only in a user or assistant
        // message. A tool message that carries one is text to the model, and a
        // strict provider rejects it outright. This assertion is the guard
        // against a later simplification that hoists inside the tool message.
        let dir = tempfile::TempDir::new().unwrap();
        let image = stored_image(&dir, "shot.png");
        let call = ToolCall::new("call-1", "read_file", json!({}));
        let mut prompt = request();

        prompt.messages = vec![
            Message::assistant(vec![t8ful_core::Content::ToolCall(call.clone())]),
            Message::tool_results([ToolResult::new(
                &call,
                ToolStatus::Ok,
                "shot.png · image/png · 1200x800",
                false,
            )
            .with_images(vec![image])]),
        ];

        let body = request_body(&prompt, &ProtocolProfile::OPENROUTER).unwrap();
        let messages = body["messages"].as_array().unwrap();
        let roles = messages
            .iter()
            .map(|message| message["role"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            roles,
            ["assistant", "tool", "user"],
            "the batch is closed before the image message: {body}"
        );

        for message in messages {
            if message["role"] == json!("tool") {
                assert!(
                    !message.to_string().contains("image_url"),
                    "a tool message must stay textual: {message}"
                );
            }
        }

        let image_messages = messages
            .iter()
            .filter(|message| message.to_string().contains("image_url"))
            .count();

        assert_eq!(image_messages, 1, "one image message per batch: {body}");
        assert_eq!(
            messages[2]["content"][0]["image_url"]["url"],
            json!(format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(b"pixels")
            ))
        );
    }

    #[test]
    fn a_tool_batch_with_several_results_keeps_them_together() {
        // Two results in one batch must both directly follow the assistant
        // message, and the image message must come after both.
        let dir = tempfile::TempDir::new().unwrap();
        let first = ToolCall::new("call-1", "read_file", json!({}));
        let second = ToolCall::new("call-2", "read_file", json!({}));
        let mut prompt = request();

        prompt.messages = vec![
            Message::assistant(vec![
                t8ful_core::Content::ToolCall(first.clone()),
                t8ful_core::Content::ToolCall(second.clone()),
            ]),
            Message::tool_results([
                ToolResult::new(&first, ToolStatus::Ok, "a.txt", false),
                ToolResult::new(&second, ToolStatus::Ok, "shot.png", false)
                    .with_images(vec![stored_image(&dir, "shot.png")]),
            ]),
        ];

        let body = request_body(&prompt, &ProtocolProfile::OPENCODE_GO).unwrap();
        let roles = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(roles, ["assistant", "tool", "tool", "user"], "{body}");
    }

    #[test]
    fn an_image_batch_is_flushed_before_a_later_user_message() {
        // The hoisted message must not swallow later history: a follow-up turn
        // still starts from its own user message.
        let dir = tempfile::TempDir::new().unwrap();
        let call = ToolCall::new("call-1", "read_file", json!({}));
        let mut prompt = request();

        prompt.messages = vec![
            Message::assistant(vec![t8ful_core::Content::ToolCall(call.clone())]),
            Message::tool_results([ToolResult::new(&call, ToolStatus::Ok, "shot.png", false)
                .with_images(vec![stored_image(&dir, "shot.png")])]),
            Message::user_text("what changed?"),
        ];

        let body = request_body(&prompt, &ProtocolProfile::OPENROUTER).unwrap();
        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 4, "{body}");
        assert_eq!(messages[2]["role"], json!("user"));
        assert!(messages[2].to_string().contains("image_url"));
        assert_eq!(messages[3]["role"], json!("user"));
        assert_eq!(messages[3]["content"], json!("what changed?"));
    }
}
