use awaken_contract::contract::commit_coordinator::CheckpointCommitPlan;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::message::{Message, Role};
use awaken_contract::contract::storage::{
    MessageSeqRange, RunMessageInput, RunMessageOutput, RunOutcome, RunRecord, RunWaitingState,
    ThreadRunStore, WaitingReason,
};
use awaken_contract::now_ms;
use awaken_contract::state::PersistedState;
use serde_json::Value;

use crate::loop_runner::AgentLoopError;

fn waiting_reason_from_backend_status(status_reason: Option<&str>) -> WaitingReason {
    match status_reason {
        Some("input_required" | "user_input_required") => WaitingReason::UserInput,
        Some("auth_required" | "suspended") => WaitingReason::ToolPermission,
        Some("awaiting_tasks") => WaitingReason::BackgroundTasks,
        Some("rate_limit") => WaitingReason::RateLimit,
        Some("manual_pause") => WaitingReason::ManualPause,
        _ => WaitingReason::ExternalEvent,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn persist_remote_root_checkpoint(
    storage: Option<&dyn ThreadRunStore>,
    thread_id: &str,
    run_id: &str,
    agent_id: &str,
    parent_run_id: Option<String>,
    run_created_at: u64,
    messages: &[Message],
    input_message_count: usize,
    status: RunStatus,
    termination_reason: Option<TerminationReason>,
    status_reason: Option<String>,
    final_output: Option<String>,
    error_payload: Option<Value>,
    run_identity: &RunIdentity,
    steps: usize,
    state: Option<PersistedState>,
    commit: crate::loop_runner::CommitWiring<'_>,
) -> Result<(), AgentLoopError> {
    let Some(storage) = storage else {
        return Ok(());
    };
    let previous = storage
        .load_run(run_id)
        .await
        .map_err(|error| AgentLoopError::StorageError(error.to_string()))?;
    let created_at = previous
        .as_ref()
        .map(|record| record.created_at)
        .unwrap_or(run_created_at / 1000);
    let finished_at = status.is_terminal().then_some(now_ms() / 1000);
    let outcome = termination_reason
        .clone()
        .map(|termination_reason| RunOutcome {
            termination_reason,
            final_output: final_output.clone(),
            error_payload: error_payload.clone(),
        });
    let waiting = (status == RunStatus::Waiting).then(|| RunWaitingState {
        reason: waiting_reason_from_backend_status(status_reason.as_deref()),
        ticket_ids: Vec::new(),
        tickets: Vec::new(),
        since_dispatch_id: run_identity.trace.dispatch_id.clone(),
        message: status_reason.clone(),
    });
    let (messages, input, output) = materialize_remote_message_log(
        messages.to_vec(),
        previous.as_ref(),
        run_identity,
        steps,
        input_message_count,
    );
    let record = RunRecord {
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        agent_id: agent_id.to_string(),
        parent_run_id,
        registry_manifest: previous
            .as_ref()
            .and_then(|record| record.registry_manifest.clone()),
        activation: previous
            .as_ref()
            .and_then(|record| record.activation.clone()),
        request: previous.as_ref().and_then(|record| record.request.clone()),
        input,
        output,
        status,
        termination_reason,
        final_output,
        error_payload,
        dispatch_id: run_identity.trace.dispatch_id.clone(),
        session_id: run_identity.trace.session_id.clone(),
        transport_request_id: run_identity.trace.transport_request_id.clone(),
        waiting,
        outcome,
        created_at,
        started_at: previous
            .as_ref()
            .and_then(|record| record.started_at)
            .or(Some(run_created_at / 1000)),
        finished_at,
        updated_at: now_ms() / 1000,
        steps,
        input_tokens: 0,
        output_tokens: 0,
        state,
    };
    let coordinator = commit.commit_coordinator.ok_or_else(|| {
        AgentLoopError::StorageError(
            "remote checkpoint requires CommitCoordinator when checkpoint_store is present"
                .to_string(),
        )
    })?;
    let plan = CheckpointCommitPlan::checkpoint_only(thread_id.to_string(), messages, record);
    coordinator
        .commit_checkpoint(plan)
        .await
        .map(|_| ())
        .map_err(|error| AgentLoopError::StorageError(error.to_string()))
}
fn materialize_remote_message_log(
    mut messages: Vec<Message>,
    previous: Option<&RunRecord>,
    run_identity: &RunIdentity,
    steps: usize,
    input_message_count: usize,
) -> (
    Vec<Message>,
    Option<RunMessageInput>,
    Option<RunMessageOutput>,
) {
    let input = previous
        .and_then(|record| record.input.clone())
        .or_else(|| {
            infer_remote_input_from_initial_messages(&run_identity.thread_id, input_message_count)
        });
    let output_start_seq = input
        .as_ref()
        .and_then(|input| input.range)
        .map(|range| range.to_seq.saturating_add(1))
        .unwrap_or(input_message_count as u64 + 1);
    let step_index = (steps > 0).then_some(steps.saturating_sub(1) as u32);
    let mut output_message_ids = Vec::new();
    let mut output_from_seq = None;
    let mut output_to_seq = None;
    for (index, message) in messages.iter_mut().enumerate() {
        let seq = index as u64 + 1;
        if seq < output_start_seq || !is_remote_run_output_message(message) {
            continue;
        }
        message.mark_produced_by(&run_identity.run_id, step_index);
        output_from_seq.get_or_insert(seq);
        output_to_seq = Some(seq);
        if let Some(id) = message.id.clone() {
            output_message_ids.push(id);
        }
    }
    let output = if output_from_seq.is_none() {
        previous.and_then(|record| record.output.clone())
    } else {
        Some(RunMessageOutput {
            thread_id: run_identity.thread_id.clone(),
            range: output_from_seq
                .zip(output_to_seq)
                .and_then(|(from, to)| MessageSeqRange::new(from, to)),
            message_ids: output_message_ids,
        })
    };
    (messages, input, output)
}
fn infer_remote_input_from_initial_messages(
    thread_id: &str,
    input_message_count: usize,
) -> Option<RunMessageInput> {
    if input_message_count == 0 {
        return None;
    }
    let to_seq = input_message_count as u64;
    Some(RunMessageInput {
        thread_id: thread_id.to_string(),
        range: MessageSeqRange::new(1, to_seq),
        trigger_message_ids: Vec::new(),
        selected_message_ids: Vec::new(),
        context_policy: None,
        compacted_snapshot_id: None,
    })
}

fn is_remote_run_output_message(message: &Message) -> bool {
    matches!(message.role, Role::Assistant | Role::Tool)
}
