//! Anthropic-native content projection within the canonical genai adapter.
//!
//! `ContentBlock` remains the neutral source of truth. This module owns only
//! the raw provider shapes that `genai` does not model as first-class parts.

use awaken_agent_contract::agent::content::{ContentBlock, DocumentSource, ImageSource};
use awaken_runtime_contract::llm::{Error, Result};

pub(crate) fn document(
    source: &DocumentSource,
    title: Option<&str>,
    context: Option<&str>,
) -> Result<serde_json::Value> {
    let source = match source {
        DocumentSource::Base64 { media_type, data } => serde_json::json!({
            "type": "base64",
            "media_type": media_type,
            "data": data,
        }),
        DocumentSource::Text { media_type, data } => serde_json::json!({
            "type": "text",
            "media_type": media_type,
            "data": data,
        }),
        DocumentSource::Url { url } => serde_json::json!({
            "type": "url",
            "url": url,
        }),
        DocumentSource::File { file_id } => {
            return Err(Error::InvalidRequest(format!(
                "File {file_id} was not materialized before provider dispatch"
            )));
        }
    };
    let mut document = serde_json::json!({
        "type": "document",
        "source": source,
    });
    let object = document
        .as_object_mut()
        .expect("the closed Anthropic document projection is an object");
    if let Some(title) = title {
        object.insert("title".into(), serde_json::Value::String(title.into()));
    }
    if let Some(context) = context {
        object.insert("context".into(), serde_json::Value::String(context.into()));
    }
    Ok(document)
}

pub(crate) fn tool_result_content(block: &ContentBlock) -> Result<serde_json::Value> {
    match block {
        ContentBlock::Text { text } => Ok(serde_json::json!({
            "type": "text",
            "text": text,
        })),
        ContentBlock::Image { source } => {
            let source = match source {
                ImageSource::Base64 { media_type, data } => serde_json::json!({
                    "type": "base64",
                    "media_type": media_type,
                    "data": data,
                }),
                ImageSource::Url { url } => serde_json::json!({
                    "type": "url",
                    "url": url,
                }),
                ImageSource::File { file_id } => {
                    return Err(Error::InvalidRequest(format!(
                        "File {file_id} was not materialized before provider dispatch"
                    )));
                }
            };
            Ok(serde_json::json!({
                "type": "image",
                "source": source,
            }))
        }
        ContentBlock::Document {
            source,
            title,
            context,
        } => document(source, title.as_deref(), context.as_deref()),
        ContentBlock::SearchResult {
            source,
            title,
            content,
            citations,
        } => Ok(serde_json::json!({
            "type": "search_result",
            "source": source,
            "title": title,
            "content": content,
            "citations": citations,
        })),
        ContentBlock::ToolReference { tool_name } => Ok(serde_json::json!({
            "type": "tool_reference",
            "tool_name": tool_name,
        })),
        ContentBlock::Redacted | ContentBlock::Thinking { .. } => Err(Error::InvalidRequest(
            "Anthropic tool results cannot contain redacted or thinking blocks".into(),
        )),
        ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => {
            Err(Error::InvalidRequest(
                "Anthropic tool results cannot contain nested tool protocol blocks".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_reference_uses_anthropics_native_closed_shape() {
        // Claude consumes a typed tool_reference; no prompt parser or free-form
        // JSON convention participates in deferred-tool discovery.
        assert_eq!(
            tool_result_content(&ContentBlock::tool_reference("create_issue"))
                .expect("tool reference projection"),
            serde_json::json!({
                "type": "tool_reference",
                "tool_name": "create_issue"
            })
        );
    }
}
