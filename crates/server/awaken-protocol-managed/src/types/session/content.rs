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

fn is_message_content(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Text { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::Document { .. }
            | ContentBlock::Redacted
    )
}

/// Project neutral agent-to-agent content onto the Managed message-content
/// union. Provider reasoning, tool protocol, and search-result blocks remain in
/// the authoritative Thread transcript but cannot cross this public wire.
pub(crate) fn project_thread_message_content(blocks: &[ContentBlock]) -> Vec<ContentBlock> {
    blocks
        .iter()
        .filter(|block| is_message_content(block))
        .cloned()
        .collect()
}

/// Apply the Managed advisor client's visibility policy after the ordinary
/// message-content closed-union projection. Runtime keeps the plaintext child
/// transcript so the coordinator can consume it; this wire boundary is the
/// sole place where redacted advisor families replace readable content.
pub(crate) fn project_advisor_thread_message_content(
    model: &str,
    blocks: &[ContentBlock],
) -> Vec<ContentBlock> {
    if blocks.is_empty() {
        return Vec::new();
    }
    if crate::types::agent::managed_advisor_result_is_redacted(model) {
        return vec![ContentBlock::Redacted];
    }
    project_thread_message_content(blocks)
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

    #[test]
    fn thread_message_projection_preserves_only_the_public_closed_union() {
        // Causes: the fixtures below establish `thread message projection` with the concrete
        // inputs, state, dependencies, and failure triggers used by this case.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1 neutral content is text/image/document/redacted;
        // C2 it is provider reasoning, tool protocol, or search-result content;
        // C3 valid and invalid blocks are interleaved. Effects: E1 preserve every
        // C1 block in exact order; E2 omit every C2 block without exposing hidden
        // reasoning or inventing replacement text. Decision rules: P1=C1=>E1,
        // P2=C2=>E2, P3=C1+C2+C3=>E1+E2.
        let blocks = vec![
            ContentBlock::thinking("private chain of thought"),
            ContentBlock::text("public answer"),
            ContentBlock::tool_use("call", "lookup", serde_json::json!({})),
            ContentBlock::Redacted,
            ContentBlock::SearchResult {
                source: "https://example.test".into(),
                title: "result".into(),
                content: vec![
                    awaken_agent_contract::agent::content::SearchResultContent::text("tool-only"),
                ],
                citations: awaken_agent_contract::agent::content::SearchResultCitations {
                    enabled: true,
                },
            },
        ];
        assert_eq!(
            project_thread_message_content(&blocks),
            vec![ContentBlock::text("public answer"), ContentBlock::Redacted],
            "P1-P3/E1-E2"
        );
    }

    #[test]
    fn advisor_message_projection_applies_the_model_visibility_decision_table() {
        // Causes: the fixtures below establish `advisor message projection applies the model
        // visibility decision table` with the concrete inputs, state, dependencies, and failure
        // triggers used by this case.
        // Effects: the observable result `all output, state, side-effect, error, and terminal
        // assertions below hold together` and every asserted state transition or side effect must
        // hold.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1 a plaintext advisor family returns public
        // content; C2 a redacted family (canonical, dated, or route-qualified)
        // returns that same private advice; C3 a plaintext transcript Message
        // has only provider-private blocks; C4 a redacted transcript Message has
        // only provider-private blocks; C5 the model is unknown. E1 preserves
        // the legal plaintext union without thinking; E2 emits exactly one
        // payloadless redacted block; E3 emits no client message; E4 fails closed
        // to E2. Runtime transcript plaintext is intentionally unchanged, and
        // the sibling ordinary-Thread test owns the unaffected child path.
        //
        // | Rule | Family | Public block exists | Effect |
        // |---|---|---|---|
        // | V1 | plaintext | yes | E1 |
        // | V2 | redacted alias | yes | E2 |
        // | V3 | plaintext | no | E3 |
        // | V4 | redacted | no | E2 |
        // | V5 | unknown | any | E4 |
        let advice = vec![
            ContentBlock::thinking("private reasoning"),
            ContentBlock::text("readable advice"),
        ];
        assert_eq!(
            project_advisor_thread_message_content("claude-opus-4-8", &advice),
            vec![ContentBlock::text("readable advice")],
            "V1/E1"
        );
        for model in [
            "claude-opus-5",
            "claude-opus-5-20260701",
            "claude-mythos-5;provider=anthropic",
            "claude-fable-5-20260701",
        ] {
            assert_eq!(
                project_advisor_thread_message_content(model, &advice),
                vec![ContentBlock::Redacted],
                "V2/E2 {model}"
            );
        }
        assert!(
            project_advisor_thread_message_content(
                "claude-opus-4-8",
                &[ContentBlock::thinking("private only")]
            )
            .is_empty(),
            "V3/E3"
        );
        assert_eq!(
            project_advisor_thread_message_content(
                "claude-opus-5",
                &[ContentBlock::thinking("private only")]
            ),
            vec![ContentBlock::Redacted],
            "V4/E2"
        );
        assert_eq!(
            project_advisor_thread_message_content("unknown-advisor", &advice),
            vec![ContentBlock::Redacted],
            "V5/E4"
        );
    }
}
