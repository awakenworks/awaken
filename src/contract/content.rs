//! Multimodal content types for messages, system prompts, and tool results.
//!
//! All content is `Vec<ContentBlock>`. A text-only message is
//! `vec![ContentBlock::text("hello")]`. No wrapper enum, no special cases.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    Document {
        source: DocumentSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
    },
    Thinking {
        thinking: String,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
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

    pub fn document_base64(
        media_type: impl Into<String>,
        data: impl Into<String>,
        title: Option<String>,
    ) -> Self {
        Self::Document {
            source: DocumentSource::Base64 {
                media_type: media_type.into(),
                data: data.into(),
            },
            title,
        }
    }
}

/// Image data source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

/// Document data source (PDF, etc.).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DocumentSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

/// Extract concatenated text from content blocks.
pub fn extract_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_block_serde_roundtrip() {
        let block = ContentBlock::text("hello");
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json, json!({"type": "text", "text": "hello"}));
        let parsed: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, block);
    }

    #[test]
    fn image_url_block_serde_roundtrip() {
        let block = ContentBlock::image_url("https://example.com/img.png");
        let json = serde_json::to_value(&block).unwrap();
        let parsed: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, block);
    }

    #[test]
    fn document_block_serde_roundtrip() {
        let block =
            ContentBlock::document_base64("application/pdf", "JVBER", Some("report.pdf".into()));
        let json = serde_json::to_value(&block).unwrap();
        let parsed: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, block);
    }

    #[test]
    fn extract_text_concatenates_text_blocks() {
        let blocks = vec![
            ContentBlock::text("hello "),
            ContentBlock::image_url("img.png"),
            ContentBlock::text("world"),
        ];
        assert_eq!(extract_text(&blocks), "hello world");
    }

    #[test]
    fn extract_text_empty_for_no_text_blocks() {
        let blocks = vec![ContentBlock::image_url("img.png")];
        assert_eq!(extract_text(&blocks), "");
    }

    #[test]
    fn extract_text_empty_for_empty_vec() {
        assert_eq!(extract_text(&[]), "");
    }

    #[test]
    fn content_blocks_array_serde_roundtrip() {
        let blocks = vec![
            ContentBlock::text("Look:"),
            ContentBlock::image_url("https://example.com/img.png"),
        ];
        let json = serde_json::to_value(&blocks).unwrap();
        assert!(json.is_array());
        let parsed: Vec<ContentBlock> = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, blocks);
    }
}
