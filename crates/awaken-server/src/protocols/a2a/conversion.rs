use awaken_contract::contract::content::{
    AudioSource, ContentBlock, DocumentSource, ImageSource, VideoSource,
};
use awaken_contract::contract::message::{
    Message as AwakenMessage, Role as AwakenRole, Visibility,
};
use awaken_protocol_a2a::{Artifact, Message as A2aMessage, MessageRole, Part};
use uuid::Uuid;

use crate::message_convert::{
    content_block_from_media_base64, content_block_from_media_url, infer_media_type_from_url,
};

use super::error::A2aError;

pub(super) fn a2a_part_to_content_block(part: &Part) -> Result<ContentBlock, A2aError> {
    if let Some(text) = part.text.as_ref() {
        return Ok(ContentBlock::text(text.clone()));
    }
    if let Some(data) = part.data.as_ref() {
        return Ok(ContentBlock::text(data.to_string()));
    }
    if let Some(url) = part.url.as_ref() {
        return Ok(url_part_to_content_block(url, part));
    }
    if let Some(raw) = part.raw.as_ref() {
        return Ok(raw_part_to_content_block(raw, part));
    }
    Err(A2aError::invalid(
        "message.parts",
        "each part must contain a supported payload",
    ))
}

fn url_part_to_content_block(url: &str, part: &Part) -> ContentBlock {
    content_block_from_media_url(url, part.media_type.as_deref(), part.filename.clone())
}

fn raw_part_to_content_block(raw: &str, part: &Part) -> ContentBlock {
    let media_type = part
        .media_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());
    content_block_from_media_base64(raw, media_type, part.filename.clone())
}

pub(super) fn awaken_message_to_a2a_message(
    message: &AwakenMessage,
    task_id: &str,
    context_id: &str,
) -> Option<A2aMessage> {
    if message.visibility == Visibility::Internal {
        return None;
    }

    let role = match message.role {
        AwakenRole::User => MessageRole::User,
        AwakenRole::Assistant => MessageRole::Agent,
        _ => return None,
    };

    let parts = message
        .content
        .iter()
        .filter_map(content_block_to_a2a_part)
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return None;
    }

    Some(A2aMessage {
        task_id: Some(task_id.to_string()),
        context_id: Some(context_id.to_string()),
        message_id: message
            .id
            .clone()
            .unwrap_or_else(|| Uuid::now_v7().to_string()),
        role,
        parts,
        metadata: None,
    })
}

fn content_block_to_a2a_part(block: &ContentBlock) -> Option<Part> {
    match block {
        ContentBlock::Text { text } => Some(Part::text(text.clone())),
        ContentBlock::Image { source } => match source {
            ImageSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: None,
                metadata: None,
            }),
            ImageSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: None,
                metadata: None,
            }),
        },
        ContentBlock::Document { source, title } => match source {
            DocumentSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: title.clone(),
                metadata: None,
            }),
            DocumentSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: title.clone(),
                metadata: None,
            }),
        },
        ContentBlock::Audio { source } => match source {
            AudioSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: None,
                metadata: None,
            }),
            AudioSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: None,
                metadata: None,
            }),
        },
        ContentBlock::Video { source } => match source {
            VideoSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: None,
                metadata: None,
            }),
            VideoSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: None,
                metadata: None,
            }),
        },
        _ => None,
    }
}

pub(super) fn message_to_artifacts(message: &A2aMessage) -> Vec<Artifact> {
    if message.parts.is_empty() {
        Vec::new()
    } else {
        vec![Artifact {
            artifact_id: "response".to_string(),
            name: Some("response".to_string()),
            description: None,
            parts: message.parts.clone(),
            metadata: None,
        }]
    }
}
