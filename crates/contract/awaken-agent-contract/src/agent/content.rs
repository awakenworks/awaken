//! Multimodal message content.
//!
//! All message content is `Vec<ContentBlock>` — there is no wrapper enum and no
//! text-only special case: a text message is `vec![ContentBlock::text("hi")]`.
//! A block is neutral, strongly-typed data (G3): it holds text, or an image by
//! reference or inline bytes — never an executable handle or untyped JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One unit of message content. A turn — user or assistant — is a list of these,
/// so text, an image, a tool request, and a tool result can interleave in a
/// single message. Further media (audio, video, documents) are added when their
/// mapping and consumer exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    /// A model's request to invoke a tool, interleaved with text in the same
    /// assistant turn. `input` is the only place untyped JSON is unavoidable: a
    /// tool's arguments are shaped by that tool's own schema, not by any type the
    /// framework can know — so they are validated against the tool schema, not
    /// statically typed here. No permission grant is implied (authority lives
    /// behind the gate).
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// A tool's result fed back to the model, addressed by the originating
    /// `ToolUse` id. Its own content is blocks, so a tool may return text today
    /// and an image later without a schema change.
    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
    },
}

/// Where image bytes come from: inline base64, or a URL the provider fetches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image_url(url: impl Into<String>) -> Self {
        Self::Image {
            source: ImageSource::Url { url: url.into() },
        }
    }

    pub fn image_base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Image {
            source: ImageSource::Base64 {
                media_type: media_type.into(),
                data: data.into(),
            },
        }
    }

    pub fn tool_use(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    pub fn tool_result(tool_use_id: impl Into<String>, content: Vec<ContentBlock>) -> Self {
        Self::ToolResult {
            tool_use_id: tool_use_id.into(),
            content,
        }
    }
}

/// A plain-text view of content: the text of every `Text` block, plus the text
/// nested inside any `ToolResult`. Images and tool-use arguments contribute no
/// text. Used for logging, digests, and a tool result's text payload.
pub fn extract_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => out.push_str(text),
            ContentBlock::ToolResult { content, .. } => out.push_str(&extract_text(content)),
            ContentBlock::Image { .. } | ContentBlock::ToolUse { .. } => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_round_trip_with_a_tagged_shape() {
        let blocks = vec![
            ContentBlock::text("look:"),
            ContentBlock::image_base64("image/png", "iVBORw0KGgo="),
            ContentBlock::image_url("https://example.test/a.jpg"),
        ];
        let json = serde_json::to_value(&blocks).expect("serialize");
        assert_eq!(json[0]["type"], "text");
        assert_eq!(json[1]["type"], "image");
        assert_eq!(json[1]["source"]["type"], "base64");
        assert_eq!(json[1]["source"]["media_type"], "image/png");
        assert_eq!(json[2]["source"]["type"], "url");

        let back: Vec<ContentBlock> = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, blocks);
    }

    #[test]
    fn extract_text_concatenates_text_and_ignores_media() {
        let blocks = vec![
            ContentBlock::text("a"),
            ContentBlock::image_url("https://example.test/x.png"),
            ContentBlock::text("b"),
        ];
        assert_eq!(extract_text(&blocks), "ab");
    }

    #[test]
    fn extract_text_recurses_into_tool_result_and_ignores_tool_use() {
        // Text nested inside a ToolResult contributes; a ToolUse's args do not.
        let blocks = vec![
            ContentBlock::text("head-"),
            ContentBlock::tool_use("c1", "run", serde_json::json!({"secret": "x"})),
            ContentBlock::tool_result(
                "c1",
                vec![
                    ContentBlock::text("nested-"),
                    ContentBlock::image_url("https://example.test/y.png"),
                    ContentBlock::text("tail"),
                ],
            ),
        ];
        assert_eq!(extract_text(&blocks), "head-nested-tail");
    }

    #[test]
    fn image_source_url_and_base64_round_trip_tagged() {
        let src = ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AA==".into(),
        };
        let json = serde_json::to_value(&src).unwrap();
        assert_eq!(json["type"], "base64");
        let back: ImageSource = serde_json::from_value(json).unwrap();
        assert_eq!(back, src);
    }
}
