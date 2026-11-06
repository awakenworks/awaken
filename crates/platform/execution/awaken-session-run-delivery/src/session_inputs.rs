// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: An answer resumes exactly the awaiting ticket it names, with the result kind that ticket's
// @formal reason: target admits; a resume activation carries only the ticket's own origin, subject and
// @formal reason: inherited tool sources. Classification, not a formal-proof claim.
//! The inputs a Session authors into one of its Runs: the result an answer to
//! an awaiting ticket resumes the Run with, the Run activation a resume
//! executes, and a Session-authored System message.

use awaken_agent_contract::agent::awaiting::{AwaitTarget, ResumeTicket};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runs::RunError;
use awaken_runtime_contract::{ExecutableAgentSnapshot, RunActivation};
use awaken_tool_contract::tool::ToolOutput;

/// The resume activation of `ticket`'s Run on `thread`: the ticket's own
/// delegation origin, data subject and inherited tool sources over `snapshot`.
#[must_use]
pub fn resume_activation(
    thread: ThreadId,
    snapshot: ExecutableAgentSnapshot,
    ticket: &ResumeTicket,
) -> RunActivation {
    let mut activation = RunActivation::new(ticket.run_id.clone(), thread, snapshot, Vec::new());
    activation.delegation_origin = ticket.delegation_origin.clone();
    activation.data_subject_id = ticket
        .data_subject_id
        .clone()
        .map(awaken_runtime_contract::DataSubjectId);
    activation.with_inherited_toolsets(ticket.inherited_toolsets.clone())
}

pub fn client_result_for_ticket(
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

pub fn session_thread_reply_result(
    ticket: &ResumeTicket,
    tool_use_id: &str,
    reply: &awaken_session_contract::SessionThreadToolReply,
) -> ResumeResult {
    match reply {
        awaken_session_contract::SessionThreadToolReply::Input(text) => {
            ResumeResult::Input(text.clone())
        }
        awaken_session_contract::SessionThreadToolReply::Confirm(decision) => {
            ResumeResult::Permission(decision.clone())
        }
        awaken_session_contract::SessionThreadToolReply::Custom { content, is_error }
        | awaken_session_contract::SessionThreadToolReply::Result { content, is_error } => {
            client_result_for_ticket(ticket, tool_use_id, content.clone(), *is_error)
        }
    }
}

/// Lower one stable Session System input into its sole durable Message form.
/// Fresh Run admission and same-Run tool reply resume share this constructor so
/// System identity, validation, and Role ordering cannot diverge.
pub fn session_system_message(
    session_id: &str,
    system: &awaken_session_contract::SessionUserRunSystemInput,
) -> Result<Message, RunError> {
    if session_id.trim().is_empty()
        || system.operation_id.trim().is_empty()
        || system.content.is_empty()
    {
        return Err(RunError::bad_request("Session System input is incomplete"));
    }
    Ok(Message::new(
        MessageId::session_system(session_id, &system.operation_id),
        Role::System,
        system.content.clone(),
    ))
}
