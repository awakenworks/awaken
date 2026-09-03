//! Official ACP codec projection (ADR-0041 Slice 3, `real-acp` feature).
//!
//! The anti-corruption layer's core: map the official `agent-client-protocol`
//! `SessionUpdate` / `StopReason` onto this crate's staging [`AcpProjectedEvent`] /
//! [`TerminationReason`]. Substituting this for the newline-JSON stand-in leaves the
//! projection + [`crate::RunFactAppender`] contract unchanged — the store never sees
//! ACP vocabulary. Pure data mapping, so it is unit-tested without a live agent.

use agent_client_protocol::{
    ContentBlock, SessionUpdate, StopReason, ToolCallContent, ToolCallStatus,
};

use crate::{AcpProjectedEvent, TerminationReason};

/// Project one ACP `SessionUpdate` into an [`AcpProjectedEvent`]. Returns `None` for
/// updates with no runtime projection (thoughts, plans, mode/config updates, user
/// input echoes, and non-terminal tool-call progress).
#[must_use]
pub fn project_update(update: &SessionUpdate) -> Option<AcpProjectedEvent> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            text_of(&chunk.content).map(|text| AcpProjectedEvent::Message {
                text,
                message_id: chunk.message_id.clone(),
            })
        }
        SessionUpdate::ToolCall(tool_call) => Some(AcpProjectedEvent::ToolCall {
            id: tool_call.tool_call_id.0.to_string(),
            name: tool_call.title.clone(),
            // Carry the tool's raw arguments through the ACL (the model's request).
            input: crate::exact_mcp_tool_arguments(
                &tool_call.title,
                &tool_call
                    .raw_input
                    .clone()
                    .unwrap_or(serde_json::Value::Null),
            ),
        }),
        // A tool-call update surfaces the result once the call reaches a terminal
        // status. The external agent runs the tool in its own OS jail, so this text
        // is the only view we get — but it *is* a real fact, so it joins the neutral
        // transcript addressed to the originating call. Non-terminal updates
        // (pending/in_progress) carry no result yet.
        SessionUpdate::ToolCallUpdate(update) => {
            let status = update.fields.status?;
            let is_error = match status {
                ToolCallStatus::Completed => false,
                ToolCallStatus::Failed => true,
                // Pending/in-progress (or any future non-terminal status) carry no
                // result yet — nothing to surface.
                _ => return None,
            };
            let content = update
                .fields
                .content
                .as_ref()
                .map(|blocks| text_of_tool_content(blocks))
                .unwrap_or_default();
            Some(AcpProjectedEvent::ToolResult {
                id: update.tool_call_id.0.to_string(),
                content,
                is_error,
            })
        }
        // Thoughts, plans, mode/config/session-info and user echoes have no runtime
        // AcpProjectedEvent projection.
        _ => None,
    }
}

fn text_of(content: &ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    }
}

/// Concatenate the text of a tool call's content blocks (a tool result may be
/// several blocks). Diffs and embedded terminals have no plain-text projection.
fn text_of_tool_content(blocks: &[ToolCallContent]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ToolCallContent::Content(content) => text_of(&content.content),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Map an ACP prompt `StopReason` to a neutral [`TerminationReason`].
#[must_use]
pub fn termination_from_stop_reason(reason: StopReason) -> TerminationReason {
    match reason {
        StopReason::EndTurn => TerminationReason::NaturalEnd,
        StopReason::Cancelled => TerminationReason::Cancelled,
        StopReason::Refusal => TerminationReason::Refusal,
        StopReason::MaxTokens | StopReason::MaxTurnRequests => TerminationReason::TimedOut,
        _ => TerminationReason::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{ContentChunk, ToolCall, ToolCallUpdate, ToolCallUpdateFields};

    #[test]
    fn agent_message_chunk_projects_to_a_neutral_message() {
        let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from("hi")));
        assert_eq!(
            project_update(&update),
            Some(AcpProjectedEvent::Message {
                text: "hi".into(),
                message_id: None,
            })
        );
    }

    #[test]
    fn agent_message_id_is_retained_as_a_transient_stream_boundary() {
        let mut chunk = ContentChunk::new(ContentBlock::from("deliverable"));
        chunk.message_id = Some("026a96a1-698c-472e-9a08-ef52a4530f79".into());
        let update = SessionUpdate::AgentMessageChunk(chunk);
        assert_eq!(
            project_update(&update),
            Some(AcpProjectedEvent::Message {
                text: "deliverable".into(),
                message_id: Some("026a96a1-698c-472e-9a08-ef52a4530f79".into()),
            })
        );
    }

    #[test]
    fn a_user_message_chunk_has_no_runtime_projection() {
        let update = SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::from("hey")));
        assert_eq!(project_update(&update), None);
    }

    #[test]
    fn a_tool_call_carries_its_id_for_later_correlation() {
        let update = SessionUpdate::ToolCall(ToolCall::new("call-7", "read"));
        assert_eq!(
            project_update(&update),
            Some(AcpProjectedEvent::ToolCall {
                id: "call-7".into(),
                name: "read".into(),
                input: serde_json::Value::Null,
            })
        );
    }

    #[test]
    fn codex_mcp_executor_envelope_projects_exact_tool_arguments() {
        let mut tool = ToolCall::new("call-mcp", "mcp.awaken_session.write");
        tool.raw_input = Some(serde_json::json!({
            "server": "awaken_session",
            "tool": "write",
            "arguments": {"path": "outputs/proof.md", "content": "ok"}
        }));
        let update = SessionUpdate::ToolCall(tool);
        assert_eq!(
            project_update(&update),
            Some(AcpProjectedEvent::ToolCall {
                id: "call-mcp".into(),
                name: "mcp.awaken_session.write".into(),
                input: serde_json::json!({"path": "outputs/proof.md", "content": "ok"}),
            })
        );

        let mut mismatch = ToolCall::new("call-mismatch", "mcp.awaken_session.write");
        mismatch.raw_input = Some(serde_json::json!({
            "server": "different",
            "tool": "write",
            "arguments": {"path": "unsafe"}
        }));
        let projected = project_update(&SessionUpdate::ToolCall(mismatch)).unwrap();
        assert!(
            matches!(projected, AcpProjectedEvent::ToolCall { input, .. } if input["server"] == "different"),
            "a mismatched envelope must remain wrapped and fail closed"
        );
    }

    #[test]
    fn a_completed_tool_call_update_projects_a_tool_result() {
        let mut fields = ToolCallUpdateFields::new();
        fields.status = Some(ToolCallStatus::Completed);
        fields.content = Some(vec![ToolCallContent::from(ContentBlock::from("file body"))]);
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("call-7", fields));
        assert_eq!(
            project_update(&update),
            Some(AcpProjectedEvent::ToolResult {
                id: "call-7".into(),
                content: "file body".into(),
                is_error: false,
            })
        );
    }

    #[test]
    fn a_failed_tool_call_update_projects_an_error_result() {
        let mut fields = ToolCallUpdateFields::new();
        fields.status = Some(ToolCallStatus::Failed);
        fields.content = Some(vec![ToolCallContent::from(ContentBlock::from("boom"))]);
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("call-9", fields));
        assert_eq!(
            project_update(&update),
            Some(AcpProjectedEvent::ToolResult {
                id: "call-9".into(),
                content: "boom".into(),
                is_error: true,
            })
        );
    }

    #[test]
    fn an_in_progress_tool_call_update_has_no_result_yet() {
        let mut fields = ToolCallUpdateFields::new();
        fields.status = Some(ToolCallStatus::InProgress);
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("call-7", fields));
        assert_eq!(project_update(&update), None);
    }

    #[test]
    fn a_completed_update_without_content_yields_an_empty_result() {
        let mut fields = ToolCallUpdateFields::new();
        fields.status = Some(ToolCallStatus::Completed);
        // No `content` field — the terminal result carries an empty string.
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("call-1", fields));
        assert_eq!(
            project_update(&update),
            Some(AcpProjectedEvent::ToolResult {
                id: "call-1".into(),
                content: String::new(),
                is_error: false,
            })
        );
    }

    #[test]
    fn a_tool_call_update_without_a_status_has_no_projection() {
        let fields = ToolCallUpdateFields::new(); // status: None
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("call-1", fields));
        assert_eq!(project_update(&update), None);
    }

    #[test]
    fn a_non_text_agent_message_chunk_has_no_projection() {
        // An agent message chunk whose content is not text (e.g. an image) has no
        // neutral text projection — the `text_of` non-text arm returns None, so no
        // Message event is produced.
        use agent_client_protocol::ImageContent;
        let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Image(
            ImageContent::new("ZGF0YQ==", "image/png"),
        )));
        assert_eq!(project_update(&update), None);
    }

    #[test]
    fn thoughts_plans_commands_and_mode_updates_all_fold_to_none() {
        // The `_ => None` catch-all swallows every non-transcript `SessionUpdate`.
        // Pin each of the four explicitly — a thought chunk, an execution plan, an
        // available-commands list, a current-mode flip — so a future projection change
        // can't silently start committing them as neutral facts (they are UI/agent
        // internals, never runtime truth). Previously none was asserted (the arm was
        // reached only implicitly by the `user_message_chunk` case).
        use agent_client_protocol::{
            AvailableCommandsUpdate, CurrentModeUpdate, Plan, SessionModeId,
        };
        let thought = SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::from(
            "let me reason about this",
        )));
        assert_eq!(project_update(&thought), None, "a thought chunk");

        let plan = SessionUpdate::Plan(Plan::new(vec![]));
        assert_eq!(project_update(&plan), None, "an execution plan");

        let commands = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![]));
        assert_eq!(
            project_update(&commands),
            None,
            "an available-commands update"
        );

        let mode =
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::new("plan")));
        assert_eq!(project_update(&mode), None, "a current-mode update");
    }

    #[test]
    fn max_turn_requests_stop_reason_maps_to_timed_out() {
        // MaxTurnRequests shares the deadline-like `TimedOut` mapping with MaxTokens;
        // it was the one un-asserted terminal stop reason.
        assert_eq!(
            termination_from_stop_reason(StopReason::MaxTurnRequests),
            TerminationReason::TimedOut
        );
    }

    #[test]
    fn stop_reasons_map_to_neutral_terminations() {
        assert_eq!(
            termination_from_stop_reason(StopReason::EndTurn),
            TerminationReason::NaturalEnd
        );
        assert_eq!(
            termination_from_stop_reason(StopReason::Cancelled),
            TerminationReason::Cancelled
        );
        assert_eq!(
            termination_from_stop_reason(StopReason::Refusal),
            TerminationReason::Refusal
        );
        assert_eq!(
            termination_from_stop_reason(StopReason::MaxTokens),
            TerminationReason::TimedOut
        );
    }
}
