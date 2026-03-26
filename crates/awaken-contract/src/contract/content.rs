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
    Audio {
        source: AudioSource,
    },
    Video {
        source: VideoSource,
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

    pub fn audio_url(url: impl Into<String>) -> Self {
        Self::Audio {
            source: AudioSource::Url { url: url.into() },
        }
    }

    pub fn audio_base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Audio {
            source: AudioSource::Base64 {
                media_type: media_type.into(),
                data: data.into(),
            },
        }
    }

    pub fn video_url(url: impl Into<String>) -> Self {
        Self::Video {
            source: VideoSource::Url { url: url.into() },
        }
    }

    pub fn video_base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Video {
            source: VideoSource::Base64 {
                media_type: media_type.into(),
                data: data.into(),
            },
        }
    }

    pub fn document_url(url: impl Into<String>, title: Option<String>) -> Self {
        Self::Document {
            source: DocumentSource::Url { url: url.into() },
            title,
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

/// Audio data source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AudioSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

/// Video data source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VideoSource {
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

    // ── Thinking block tests ──

    #[test]
    fn thinking_block_serde_roundtrip() {
        let block = ContentBlock::Thinking {
            thinking: "Let me consider...".into(),
        };
        let json_val = serde_json::to_value(&block).unwrap();
        assert_eq!(json_val["type"], "thinking");
        assert_eq!(json_val["thinking"], "Let me consider...");
        let parsed: ContentBlock = serde_json::from_value(json_val).unwrap();
        assert_eq!(parsed, block);
    }

    // ── ToolUse block tests ──

    #[test]
    fn tool_use_block_serde_roundtrip() {
        let block = ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "search".into(),
            input: json!({"query": "rust"}),
        };
        let json_val = serde_json::to_value(&block).unwrap();
        assert_eq!(json_val["type"], "tool_use");
        assert_eq!(json_val["id"], "call_1");
        assert_eq!(json_val["name"], "search");
        let parsed: ContentBlock = serde_json::from_value(json_val).unwrap();
        assert_eq!(parsed, block);
    }

    // ── ToolResult block tests ──

    #[test]
    fn tool_result_block_serde_roundtrip() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: vec![ContentBlock::text("Result: 42")],
        };
        let json_val = serde_json::to_value(&block).unwrap();
        assert_eq!(json_val["type"], "tool_result");
        assert_eq!(json_val["tool_use_id"], "call_1");
        let parsed: ContentBlock = serde_json::from_value(json_val).unwrap();
        assert_eq!(parsed, block);
    }

    // ── Image base64 tests ──

    #[test]
    fn image_base64_block_serde_roundtrip() {
        let block = ContentBlock::image_base64("image/png", "iVBORw0KGgo=");
        let json_val = serde_json::to_value(&block).unwrap();
        assert_eq!(json_val["type"], "image");
        assert_eq!(json_val["source"]["type"], "base64");
        assert_eq!(json_val["source"]["media_type"], "image/png");
        let parsed: ContentBlock = serde_json::from_value(json_val).unwrap();
        assert_eq!(parsed, block);
    }

    // ── Document block without title ──

    #[test]
    fn document_block_without_title_omits_field() {
        let block = ContentBlock::document_base64("application/pdf", "JVBER", None);
        let json_val = serde_json::to_value(&block).unwrap();
        assert!(json_val.get("title").is_none());
        let parsed: ContentBlock = serde_json::from_value(json_val).unwrap();
        assert_eq!(parsed, block);
    }

    // ── Mixed content blocks ──

    #[test]
    fn mixed_content_blocks_roundtrip() {
        let blocks = vec![
            ContentBlock::text("Here is the result:"),
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "calc".into(),
                input: json!({"expr": "2+2"}),
            },
            ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: vec![ContentBlock::text("4")],
            },
            ContentBlock::Thinking {
                thinking: "hmm".into(),
            },
        ];
        let json_val = serde_json::to_value(&blocks).unwrap();
        let parsed: Vec<ContentBlock> = serde_json::from_value(json_val).unwrap();
        assert_eq!(parsed, blocks);
    }

    #[test]
    fn content_block_debug_output() {
        let block = ContentBlock::text("hi");
        let debug = format!("{:?}", block);
        assert!(debug.contains("Text"));
        assert!(debug.contains("hi"));
    }

    #[test]
    fn audio_url_block_serde_roundtrip() {
        let block = ContentBlock::audio_url("https://example.com/audio.mp3");
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "audio");
        assert_eq!(json["source"]["type"], "url");
        assert_eq!(json["source"]["url"], "https://example.com/audio.mp3");
        let parsed: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, block);
    }

    #[test]
    fn audio_base64_block_serde_roundtrip() {
        let block = ContentBlock::audio_base64("audio/mpeg", "SGVsbG8=");
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "audio");
        assert_eq!(json["source"]["type"], "base64");
        assert_eq!(json["source"]["media_type"], "audio/mpeg");
        let parsed: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, block);
    }

    #[test]
    fn video_url_block_serde_roundtrip() {
        let block = ContentBlock::video_url("https://example.com/video.mp4");
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "video");
        assert_eq!(json["source"]["type"], "url");
        assert_eq!(json["source"]["url"], "https://example.com/video.mp4");
        let parsed: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, block);
    }

    #[test]
    fn video_base64_block_serde_roundtrip() {
        let block = ContentBlock::video_base64("video/mp4", "AAAA");
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "video");
        assert_eq!(json["source"]["type"], "base64");
        assert_eq!(json["source"]["media_type"], "video/mp4");
        let parsed: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, block);
    }

    #[test]
    fn content_block_clone() {
        let block = ContentBlock::text("hello");
        let cloned = block.clone();
        assert_eq!(block, cloned);
    }
}
