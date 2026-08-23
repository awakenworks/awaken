//! Pure projections used by Run recovery, delegation, and resume delivery.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct StepCommitExpectation<'a> {
    pub(super) messages_before: usize,
    pub(super) input_ids: &'a [String],
}

pub(super) fn project_delegated_runs(
    registry: Option<&awaken_agent_contract::agent::delegation::DelegationRegistry>,
) -> Vec<DelegatedRun> {
    registry
        .into_iter()
        .flat_map(|registry| {
            registry.delegations().map(|delegation| DelegatedRun {
                run_id: delegation.child_run_id.clone(),
                parent_call_id: delegation.parent_call_id.clone(),
                agent_id: delegation.target_agent_id.clone(),
                status: delegation.status,
            })
        })
        .collect()
}

pub(super) fn delegation_registry_from_snapshot(
    snapshot: &RunRecoverySnapshot,
    run_id: &RunId,
) -> Result<Option<awaken_agent_contract::agent::delegation::DelegationRegistry>, HostError> {
    let mut store = Store::new();
    for command in &snapshot.state {
        if command.scope == Scope::Run && command.run_id.as_ref() == Some(run_id) {
            store.apply(command);
        }
    }
    RunDelegations::load(&store).map_err(|error| HostError::internal(error.to_string()))
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
    reply: &awaken_session_contract::SessionThreadToolReply,
) -> ResumeResult {
    match reply {
        awaken_session_contract::SessionThreadToolReply::Confirm(decision) => {
            ResumeResult::Permission(decision.clone())
        }
        awaken_session_contract::SessionThreadToolReply::Custom { content, is_error }
        | awaken_session_contract::SessionThreadToolReply::Result { content, is_error } => {
            client_result_for_ticket(ticket, tool_use_id, content.clone(), *is_error)
        }
    }
}

pub(super) fn recovery_ticket(
    committed: &RunRecoverySnapshot,
    run_id: &RunId,
) -> Option<ResumeTicket> {
    committed
        .resume_tickets
        .iter()
        .find(|entry| &entry.run_id == run_id)
        .map(|entry| entry.ticket.clone())
}
