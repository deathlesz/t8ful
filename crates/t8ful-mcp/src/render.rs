//! Turn an MCP `CallToolResult` into bounded model-facing text.

use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};

/// One rendered content block, before the overall budget is applied.
pub fn render(result: &CallToolResult) -> (String, bool) {
    let mut parts = Vec::new();

    for content in &result.content {
        match content {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::Image(image) => parts.push(format!(
                "[image content: {}; {}]",
                image.mime_type,
                human_bytes(image.data.len())
            )),
            ContentBlock::Audio(audio) => parts.push(format!(
                "[audio content: {}; {}]",
                audio.mime_type,
                human_bytes(audio.data.len())
            )),
            ContentBlock::Resource(resource) => {
                parts.extend(render_resource(&resource.resource));
            }
            ContentBlock::ResourceLink(link) => parts.push(link.uri.clone()),
            // `ContentBlock` is `#[non_exhaustive]`: a newer server may send a
            // block this client does not know. It is left out rather than
            // rendered as noise the model cannot act on.
            _ => {}
        }
    }

    // `structuredContent` is the machine-readable duplicate of the text blocks.
    // It is only shown when the server sent no text at all, so a server that
    // returns both does not double its answer.
    if parts.is_empty()
        && let Some(structured) = &result.structured_content
    {
        parts.push(match serde_json::to_string_pretty(structured) {
            Ok(json) => json,
            Err(error) => format!("[unrenderable structured content: {error}]"),
        });
    }

    let joined = parts.join("\n");

    (joined, result.is_error.unwrap_or(false))
}

fn render_resource(resource: &ResourceContents) -> Option<String> {
    match resource {
        ResourceContents::TextResourceContents { uri, text, .. } => Some(format!("{uri}\n{text}")),
        ResourceContents::BlobResourceContents { uri, blob, .. } => Some(format!(
            "{uri}\n[binary content; {}]",
            human_bytes(blob.len())
        )),
        // `ResourceContents` is also `#[non_exhaustive]`. Dropping an unknown
        // shape is better than an empty part, which would add a blank line
        // between the blocks around it.
        _ => None,
    }
}

/// A short human-readable size for a base64 payload.
fn human_bytes(len: usize) -> String {
    if len >= 1024 {
        format!("{} KiB", len.div_ceil(1024))
    } else {
        format!("{len} B")
    }
}

#[cfg(test)]
mod tests {
    use super::render;
    use rmcp::model::{CallToolResult, ContentBlock, ImageContent, Resource};

    fn text(body: &str) -> ContentBlock {
        ContentBlock::text(body)
    }

    fn image(mime: &str, data: &str) -> ContentBlock {
        ContentBlock::Image(ImageContent::new(data, mime))
    }

    fn link(uri: &str) -> ContentBlock {
        ContentBlock::ResourceLink(Resource::new(uri, "res"))
    }

    fn result(content: Vec<ContentBlock>) -> CallToolResult {
        CallToolResult::success(content)
    }

    #[test]
    fn text_blocks_are_joined_in_order() {
        let (rendered, is_error) = render(&result(vec![text("first"), text("second")]));

        assert_eq!(rendered, "first\nsecond");
        assert!(!is_error);
    }

    #[test]
    fn image_and_link_blocks_become_placeholders() {
        let (rendered, _) = render(&result(vec![
            image("image/png", &"a".repeat(12 * 1024)),
            link("file:///work/notes.md"),
        ]));

        assert!(
            rendered.contains("[image content: image/png; 12 KiB]"),
            "{rendered}"
        );
        assert!(rendered.contains("file:///work/notes.md"), "{rendered}");
    }

    #[test]
    fn structured_content_is_only_used_when_no_block_was_rendered() {
        let with_text = CallToolResult::success(vec![text("prose")]);
        let (rendered, _) = render(&with_text);

        assert_eq!(rendered, "prose");

        // `CallToolResult::structured` also fills `content` with the compact
        // JSON, so the empty-text fallback is set up explicitly here.
        let mut structured = CallToolResult::success(Vec::new());
        structured.structured_content = Some(serde_json::json!({ "answer": 42 }));

        let (rendered, _) = render(&structured);

        assert!(rendered.contains("answer"), "{rendered}");
        assert!(rendered.contains("42"), "{rendered}");
    }

    #[test]
    fn an_error_result_is_reported() {
        let (rendered, is_error) = render(&CallToolResult::error(vec![text("no such file")]));

        assert_eq!(rendered, "no such file");
        assert!(is_error);
    }

    #[test]
    fn an_empty_result_renders_empty_text() {
        let (rendered, is_error) = render(&result(Vec::new()));

        assert!(rendered.is_empty());
        assert!(!is_error);
    }
}
