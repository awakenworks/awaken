//! Multimodal message content.
//!
//! All message content is `Vec<ContentBlock>` — there is no wrapper enum and no
//! text-only special case: a text message is `vec![ContentBlock::text("hi")]`.
//! A block is neutral, strongly-typed data (G3): it holds text, or an image by
//! reference or inline bytes — never an executable handle or untyped JSON.

use serde::{Deserialize, Serialize};

/// One unit of message content. The set is intentionally narrow — only the
/// modalities the runtime and a provider carry today. Tool-call and tool-result
/// blocks, and further media (audio, video, documents), are added when their
/// mapping and consumer exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { source: ImageSource },
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
}

/// Concatenate the text of every `Text` block, ignoring non-text blocks. Used
/// where a plain-text view of multimodal content is enough (logging, a digest).
pub fn extract_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text { text } = block {
            out.push_str(text);
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
}
