// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: Static owner: runtime-host Run recovery and input projection. Dynamic path: committed
// @formal reason: Run and Thread facts select the exact resume state supplied to the Agent loop after
// @formal reason: start or recovery. Evidence: runtime.agent_loop.runtime_execution and runtime-host
// @formal reason: recovery tests; this is a product-requirement classification, not a direct proof claim.
//! Pure projections used by Run recovery, delegation, and resume delivery.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct StepCommitExpectation<'a> {
    pub(super) messages_before: usize,
    pub(super) input_ids: &'a [String],
}

pub(super) fn client_result_for_ticket(
    ticket: &ResumeTicket,
    tool_use_id: &str,
    content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    is_error: bool,
) -> ResumeResult {
    if matches!(ticket.target(), AwaitTarget::RemoteInput { .. }) {
        ResumeResult::Input(awaken_agent_contract::agent::content::extract_text(
            &content,
        ))
    } else {
        let output = if is_error {
            ToolOutput::error_blocks(tool_use_id, content)
        } else {
            ToolOutput::ok_blocks(tool_use_id, content)
        };
        ResumeResult::ToolResult(output)
    }
}

pub(in crate::host) fn session_thread_reply_result(
    ticket: &ResumeTicket,
    tool_use_id: &str,
    reply: &awaken_session_execution_contract::SessionThreadToolReply,
) -> ResumeResult {
    match reply {
        awaken_session_execution_contract::SessionThreadToolReply::Input(text) => {
            ResumeResult::Input(text.clone())
        }
        awaken_session_execution_contract::SessionThreadToolReply::Confirm(decision) => {
            ResumeResult::Permission(decision.clone())
        }
        awaken_session_execution_contract::SessionThreadToolReply::Custom { content, is_error }
        | awaken_session_execution_contract::SessionThreadToolReply::Result { content, is_error } => {
            client_result_for_ticket(ticket, tool_use_id, content.clone(), *is_error)
        }
    }
}
