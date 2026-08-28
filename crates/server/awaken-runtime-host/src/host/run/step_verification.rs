//! Committed-step and resume assertions at the Host boundary.

use super::*;

impl SharedHost {
    /// Fail closed before resuming: the asserted `tool_use_id` must name the
    /// Run's pending tool and its binding must match the inbound resume kind.
    pub(in crate::host) fn check_pending(
        &self,
        ticket: &ResumeTicket,
        tool_use_id: &str,
        want_client: bool,
    ) -> Result<(), HostError> {
        let pending = Pending::from_resume_ticket(ticket)
            .ok_or_else(|| HostError::internal("awaiting run has no pending tool"))?;
        if pending.tool_use_id != tool_use_id {
            return Err(HostError::bad_request(format!(
                "tool_use_id {tool_use_id:?} does not match the pending tool"
            )));
        }
        if pending.client_executed != want_client {
            let (got, expected) = if want_client {
                ("built-in", "a confirmation")
            } else {
                ("client-executed", "a client tool result")
            };
            return Err(HostError::bad_request(format!(
                "pending tool is {got}; answer it with {expected}"
            )));
        }
        Ok(())
    }
}

/// Locate the committed prefix immediately before one exact admitted input set.
pub(super) fn message_prefix_before_exact_inputs(
    committed: &RunRecoverySnapshot,
    expected_input_ids: &[String],
) -> Result<usize, HostError> {
    if expected_input_ids.is_empty() {
        return Err(HostError::bad_request(
            "Session Run admission requires at least one input message",
        ));
    }
    let mut positions = Vec::with_capacity(expected_input_ids.len());
    for expected in expected_input_ids {
        let mut matches = committed
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.id.0 == *expected)
            .map(|(index, _)| index);
        let Some(position) = matches.next() else {
            return Err(HostError::internal(format!(
                "committed Session Run is missing input message `{expected}`"
            )));
        };
        if matches.next().is_some() {
            return Err(HostError::internal(format!(
                "committed Session Run contains duplicate input message `{expected}`"
            )));
        }
        positions.push(position);
    }
    let first = positions[0];
    if positions
        .iter()
        .copied()
        .enumerate()
        .any(|(offset, position)| position != first.saturating_add(offset))
    {
        return Err(HostError::internal(
            "committed Session Run input messages are not one exact ordered prefix",
        ));
    }
    Ok(first)
}

/// Bind executor completion to the exact durable Thread/Run state and inputs.
pub(super) fn verify_committed_step(
    committed: &RunRecoverySnapshot,
    thread_id: &ThreadId,
    run_id: &RunId,
    returned_state: &RunState,
    before: usize,
    expected_input_ids: &[String],
) -> Result<Vec<Message>, HostError> {
    if &committed.thread_id != thread_id || &committed.claimed_run_id != run_id {
        return Err(HostError::internal(
            "committed step proof names another Thread or Run",
        ));
    }
    if committed.latest_run_id.as_ref() != Some(run_id) {
        return Err(HostError::internal(
            "committed step proof is not the latest Run on its Thread",
        ));
    }
    let committed_state = committed
        .runs
        .iter()
        .find(|run| &run.id == run_id && &run.thread_id == thread_id)
        .map(|run| &run.state)
        .ok_or_else(|| HostError::internal("committed step proof has no Run record"))?;
    if committed_state != returned_state {
        return Err(HostError::internal(
            "executor result does not match the committed Run state",
        ));
    }
    if matches!(returned_state, RunState::Running) {
        return Err(HostError::internal("committed step proof is not settled"));
    }
    if committed.messages.len() < before {
        return Err(HostError::internal(
            "committed Thread message prefix moved backwards",
        ));
    }
    if committed.thread_version == 0 || committed.next_commit_ordinal == 0 {
        return Err(HostError::internal(
            "committed step proof has no durable commit identity",
        ));
    }
    for expected in expected_input_ids {
        if !committed
            .messages
            .iter()
            .any(|message| message.id.0 == *expected)
        {
            return Err(HostError::internal(format!(
                "committed step proof is missing input message `{expected}`"
            )));
        }
    }
    let suffix = committed.messages[before..].to_vec();
    // Natural completion requires committed assistant output. An error state
    // already carries its typed explanation in the authoritative RunState.
    if matches!(returned_state, RunState::Ended(EndCause::NaturalEnd))
        && !suffix.iter().any(|message| message.role == Role::Assistant)
    {
        return Err(HostError::internal(
            "committed natural terminal step has no assistant output",
        ));
    }
    Ok(suffix)
}
