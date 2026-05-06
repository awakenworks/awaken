//! Shared message conversion for protocol handlers.

use awaken_contract::contract::content::{ContentBlock, extract_text};
use awaken_contract::contract::message::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    Document,
}

/// Convert protocol-agnostic role+content pairs to Messages.
pub fn convert_role_content_pairs(
    pairs: impl IntoIterator<Item = (String, String)>,
) -> Vec<Message> {
    pairs
        .into_iter()
        .filter_map(|(role, content)| match role.as_str() {
            "user" => Some(Message::user(content)),
            "assistant" => Some(Message::assistant(content)),
            "system" => Some(Message::system(content)),
            other => {
                tracing::warn!(role = other, "dropping message with unsupported role");
                None
            }
        })
        .collect()
}

pub fn message_from_role_blocks(
    role: &str,
    blocks: Vec<ContentBlock>,
    include_assistant: bool,
) -> Option<Message> {
    if blocks.is_empty() {
        return None;
    }
    match role {
        "user" => Some(Message::user_with_content(blocks)),
        "system" => Some(Message::system(extract_text(&blocks))),
        "assistant" if include_assistant => Some(Message::assistant(extract_text(&blocks))),
        "assistant" => None,
        other => {
            tracing::warn!(role = other, "dropping message with unsupported role");
            None
        }
    }
}

pub fn parse_data_uri(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    Some((media_type.to_string(), data.to_string()))
}

pub fn infer_media_type_from_url(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".mp3") {
        "audio/mpeg"
    } else if lower.ends_with(".wav") {
        "audio/wav"
    } else if lower.ends_with(".ogg") {
        "audio/ogg"
    } else if lower.ends_with(".mp4") {
        "video/mp4"
    } else if lower.ends_with(".webm") {
        "video/webm"
    } else if lower.ends_with(".pdf") {
        "application/pdf"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".txt") || lower.ends_with(".md") {
        "text/plain"
    } else {
        "application/octet-stream"
    }
    .to_string()
}

pub fn infer_media_kind(media_type: &str) -> MediaKind {
    if media_type.starts_with("image/") {
        MediaKind::Image
    } else if media_type.starts_with("audio/") {
        MediaKind::Audio
    } else if media_type.starts_with("video/") {
        MediaKind::Video
    } else {
        MediaKind::Document
    }
}

pub fn content_block_from_url(
    kind: MediaKind,
    url: impl Into<String>,
    title: Option<String>,
) -> ContentBlock {
    let url = url.into();
    match kind {
        MediaKind::Image => ContentBlock::image_url(url),
        MediaKind::Audio => ContentBlock::audio_url(url),
        MediaKind::Video => ContentBlock::video_url(url),
        MediaKind::Document => ContentBlock::document_url(url, title),
    }
}

pub fn content_block_from_base64(
    kind: MediaKind,
    media_type: impl Into<String>,
    data: impl Into<String>,
    title: Option<String>,
) -> ContentBlock {
    let media_type = media_type.into();
    let data = data.into();
    match kind {
        MediaKind::Image => ContentBlock::image_base64(media_type, data),
        MediaKind::Audio => ContentBlock::audio_base64(media_type, data),
        MediaKind::Video => ContentBlock::video_base64(media_type, data),
        MediaKind::Document => ContentBlock::document_base64(media_type, data, title),
    }
}

pub fn content_block_from_media_url(
    url: impl Into<String>,
    media_type: Option<&str>,
    title: Option<String>,
) -> ContentBlock {
    let url = url.into();
    let media_type = media_type
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| infer_media_type_from_url(&url));
    content_block_from_url(infer_media_kind(&media_type), url, title)
}

pub fn content_block_from_media_base64(
    data: impl Into<String>,
    media_type: impl Into<String>,
    title: Option<String>,
) -> ContentBlock {
    let media_type = media_type.into();
    content_block_from_base64(infer_media_kind(&media_type), media_type, data, title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_known_roles() {
        let pairs = vec![
            ("user".into(), "hello".into()),
            ("assistant".into(), "hi".into()),
            ("system".into(), "sys".into()),
        ];
        let msgs = convert_role_content_pairs(pairs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].text(), "hello");
        assert_eq!(msgs[1].text(), "hi");
        assert_eq!(msgs[2].text(), "sys");
    }

    #[test]
    fn convert_skips_unknown_roles() {
        let pairs = vec![
            ("user".into(), "hello".into()),
            ("function".into(), "result".into()),
            ("unknown".into(), "x".into()),
        ];
        let msgs = convert_role_content_pairs(pairs);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text(), "hello");
    }

    #[test]
    fn convert_empty() {
        let msgs = convert_role_content_pairs(std::iter::empty());
        assert!(msgs.is_empty());
    }
}
