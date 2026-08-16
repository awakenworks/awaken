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
    /// A model-visible document. `File` is a logical Resources identity and is
    /// resolved by the attempt-bound content materializer before provider I/O;
    /// it is never interpreted as a local path or a provider-owned file id.
    Document {
        source: DocumentSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
    },
    /// A citation-capable search result returned by a tool. Its body is
    /// deliberately text-only: arbitrary nested blocks would create another
    /// message grammar inside a content block.
    SearchResult {
        source: String,
        title: String,
        content: Vec<SearchResultContent>,
        citations: SearchResultCitations,
    },
    /// Content withheld by model policy. It is intentionally payloadless so
    /// secret or encrypted provider data cannot leak through neutral replay.
    Redacted,
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
        #[serde(default)]
        is_error: bool,
    },
    /// The model's extended-thinking (reasoning) content, folded from the
    /// provider's reasoning stream (Axis 10b). It interleaves with `Text` in an
    /// assistant turn but is NOT part of the answer: `extract_text` ignores it,
    /// and a protocol adapter projects only its *presence* as a contentless
    /// reasoning-progress marker — the reasoning text is not on any answer wire.
    Thinking {
        text: String,
        /// Opaque provider proof required to replay signed thinking on a later
        /// assistant tool-use continuation. The neutral runtime stores but never
        /// interprets it; providers without signed thinking leave it absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

/// Where image bytes come from: inline base64, or a URL the provider fetches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
    File { file_id: String },
}

/// Provider-neutral document source. Inline text retains its required media
/// type so protocol adapters can validate an exact wire contract rather than
/// silently normalizing an unsupported value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DocumentSource {
    Base64 { media_type: String, data: String },
    Text { media_type: String, data: String },
    Url { url: String },
    File { file_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResultContent {
    #[serde(rename = "type")]
    pub kind: SearchResultContentType,
    pub text: String,
}

impl SearchResultContent {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: SearchResultContentType::Text,
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchResultContentType {
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResultCitations {
    pub enabled: bool,
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

    pub fn image_file(file_id: impl Into<String>) -> Self {
        Self::Image {
            source: ImageSource::File {
                file_id: file_id.into(),
            },
        }
    }

    pub fn document_file(file_id: impl Into<String>) -> Self {
        Self::Document {
            source: DocumentSource::File {
                file_id: file_id.into(),
            },
            title: None,
            context: None,
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
        Self::tool_result_with_error(tool_use_id, content, false)
    }

    pub fn tool_result_with_error(
        tool_use_id: impl Into<String>,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Self {
        Self::ToolResult {
            tool_use_id: tool_use_id.into(),
            content,
            is_error,
        }
    }

    pub fn thinking(text: impl Into<String>) -> Self {
        Self::Thinking {
            text: text.into(),
            signature: None,
        }
    }

    pub fn signed_thinking(text: impl Into<String>, signature: impl Into<Option<String>>) -> Self {
        Self::Thinking {
            text: text.into(),
            signature: signature.into(),
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
            ContentBlock::Image { .. }
            | ContentBlock::Document { .. }
            | ContentBlock::SearchResult { .. }
            | ContentBlock::Redacted
            | ContentBlock::ToolUse { .. }
            | ContentBlock::Thinking { .. } => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_round_trip_with_a_tagged_shape() {
        // Cause/effect decision rule T1: signed thinking is committed as neutral
        // message data (C1) -> preserve text and opaque signature through serde
        // (E1), while unsigned thinking (C2) -> omit the optional wire field and
        // deserialize it as None (E2). This keeps the message log as the one
        // replay authority without forcing provider metadata onto other models.
        let blocks = vec![
            ContentBlock::text("look:"),
            ContentBlock::image_base64("image/png", "iVBORw0KGgo="),
            ContentBlock::image_url("https://example.test/a.jpg"),
            ContentBlock::document_file("file-doc"),
            ContentBlock::SearchResult {
                source: "https://example.test/result".into(),
                title: "Result".into(),
                content: vec![SearchResultContent::text("answer")],
                citations: SearchResultCitations { enabled: true },
            },
            ContentBlock::Redacted,
            ContentBlock::signed_thinking("check", Some("proof".to_string())),
            ContentBlock::thinking("unsigned"),
        ];
        let json = serde_json::to_value(&blocks).expect("serialize");
        assert_eq!(json[0]["type"], "text");
        assert_eq!(json[1]["type"], "image");
        assert_eq!(json[1]["source"]["type"], "base64");
        assert_eq!(json[1]["source"]["media_type"], "image/png");
        assert_eq!(json[2]["source"]["type"], "url");
        assert_eq!(json[3]["source"]["type"], "file");
        assert_eq!(json[4]["type"], "search_result");
        assert_eq!(json[4]["content"][0]["type"], "text");
        assert_eq!(json[5]["type"], "redacted");
        assert_eq!(json[6]["signature"], "proof", "T1/E1");
        assert!(json[7].get("signature").is_none(), "T1/E2");

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
