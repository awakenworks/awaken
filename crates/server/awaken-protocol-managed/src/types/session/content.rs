//! Contextual admission for neutral content blocks at the Managed wire boundary.

use awaken_agent_contract::agent::content::{ContentBlock, DocumentSource, ImageSource};

use super::InboundEvent;

impl InboundEvent {
    pub(crate) fn validate_content(&self) -> Result<usize, String> {
        match self {
            Self::UserMessage { content } => {
                validate_content_blocks(content, ContentUse::UserMessage, "user.message")
            }
            Self::SystemMessage { content } => {
                validate_content_blocks(content, ContentUse::SystemMessage, "system.message")
            }
            Self::UserCustomToolResult { content, .. } | Self::UserToolResult { content, .. } => {
                validate_content_blocks(
                    content.as_deref().unwrap_or_default(),
                    ContentUse::ToolResult,
                    self.type_str(),
                )
            }
            Self::UserToolConfirmation { .. }
            | Self::UserDefineOutcome { .. }
            | Self::UserInterrupt { .. } => Ok(0),
        }
    }
}

#[derive(Clone, Copy)]
enum ContentUse {
    UserMessage,
    SystemMessage,
    ToolResult,
}

fn validate_content_blocks(
    blocks: &[ContentBlock],
    usage: ContentUse,
    event_type: &str,
) -> Result<usize, String> {
    let mut file_documents = 0usize;
    for block in blocks {
        let allowed = match usage {
            ContentUse::UserMessage => matches!(
                block,
                ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Document { .. }
                    | ContentBlock::Redacted
            ),
            ContentUse::SystemMessage => matches!(block, ContentBlock::Text { .. }),
            ContentUse::ToolResult => matches!(
                block,
                ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Document { .. }
                    | ContentBlock::SearchResult { .. }
            ),
        };
        if !allowed {
            return Err(format!(
                "{event_type} contains a content block that is not valid in this context"
            ));
        }
        match block {
            ContentBlock::Image {
                source: ImageSource::File { file_id },
            }
            | ContentBlock::Document {
                source: DocumentSource::File { file_id },
                ..
            } if file_id.trim().is_empty() => {
                return Err(format!("{event_type} contains an empty file_id"));
            }
            ContentBlock::Document {
                source: DocumentSource::File { .. },
                ..
            } => file_documents += 1,
            ContentBlock::Document {
                source: DocumentSource::Text { media_type, .. },
                ..
            } if media_type != "text/plain" => {
                return Err(format!(
                    "{event_type} plain-text document media_type must be text/plain"
                ));
            }
            ContentBlock::SearchResult { content, .. } if content.is_empty() => {
                return Err(format!(
                    "{event_type} search_result content must not be empty"
                ));
            }
            _ => {}
        }
    }
    Ok(file_documents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::session::SessionCreateParams;

    /// Cause-effect graph and FMECA for Managed content admission:
    /// C1=user block is text/image/document/redacted -> E1 accept; C2=tool-result
    /// block is text/image/document/search_result -> E1; C3=context/block mismatch
    /// -> E2 bad request before Session mutation; C4=empty File id, invalid text
    /// MIME, or empty search content -> E2; C5=file-document count <=100 -> E1;
    /// C6=count >100 -> E2. Constraints: search_result is tool-result-only;
    /// system is text-only; the 100 limit counts document File sources, not image
    /// File sources. FMECA: accepting a provider-only block in the wrong context
    /// causes late 400s (high, contextual closed union); empty/oversized references
    /// can bypass Files authorization or amplify reads (critical/high, local gates).
    /// Decision rules C1/C2/C5=M1, C3=M2, C4=M3, C6=M4.
    #[test]
    fn managed_content_contexts_and_file_document_limit_fail_closed() {
        let valid_user: InboundEvent = serde_json::from_value(serde_json::json!({
            "type":"user.message",
            "content":[
                {"type":"document","source":{"type":"file","file_id":"file-a"}},
                {"type":"redacted"}
            ]
        }))
        .unwrap();
        assert_eq!(valid_user.validate_content().unwrap(), 1, "M1");

        let valid_tool: InboundEvent = serde_json::from_value(serde_json::json!({
            "type":"user.tool_result", "tool_use_id":"call-a",
            "content":[{"type":"search_result","source":"https://example.test",
                "title":"Result","content":[{"type":"text","text":"answer"}],
                "citations":{"enabled":true}}]
        }))
        .unwrap();
        assert_eq!(valid_tool.validate_content().unwrap(), 0, "M1");

        for invalid in [
            serde_json::json!({"type":"user.message","content":[{
                "type":"search_result","source":"s","title":"t",
                "content":[{"type":"text","text":"x"}],"citations":{"enabled":true}}]}),
            serde_json::json!({"type":"system.message","content":[{
                "type":"document","source":{"type":"text","media_type":"text/plain","data":"x"}}]}),
        ] {
            let event: InboundEvent = serde_json::from_value(invalid).unwrap();
            assert!(event.validate_content().is_err(), "M2");
        }
        for invalid in [
            serde_json::json!({"type":"user.message","content":[{
                "type":"document","source":{"type":"file","file_id":""}}]}),
            serde_json::json!({"type":"user.message","content":[{
                "type":"document","source":{"type":"text","media_type":"text/markdown","data":"x"}}]}),
            serde_json::json!({"type":"user.tool_result","tool_use_id":"call-a","content":[{
                "type":"search_result","source":"s","title":"t","content":[],
                "citations":{"enabled":true}}]}),
        ] {
            let event: InboundEvent = serde_json::from_value(invalid).unwrap();
            assert!(event.validate_content().is_err(), "M3");
        }

        let create_with = |count: usize| {
            serde_json::from_value::<SessionCreateParams>(serde_json::json!({
                "agent":"assistant", "environment_id":"environment-a",
                "initial_events":[{"type":"user.message","content":
                    (0..count).map(|index| serde_json::json!({
                        "type":"document","source":{"type":"file","file_id":format!("file-{index}")}
                    })).collect::<Vec<_>>()
                }]
            }))
            .unwrap()
        };
        assert!(create_with(100).validate_initial_events().is_ok(), "M1/M5");
        assert!(create_with(101).validate_initial_events().is_err(), "M4/M6");
    }
}
