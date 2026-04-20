//! Step completion, checkpointing, state snapshots, and termination checks.

use std::sync::Arc;

use crate::hooks::PhaseContext;
use crate::phase::{ExecutionEnv, PhaseRuntime};
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::message::{Message, Role};
use awaken_contract::contract::storage::{
    MessageSeqRange, RunMessageInput, RunMessageOutput, RunOutcome, RunRecord, RunWaitingState,
    RunWaitingTicket, ThreadRunStore, WaitingReason,
};
use awaken_contract::contract::suspension::ToolCallStatus;
use awaken_contract::model::Phase;
use serde_json::Value;

use super::{AgentLoopError, commit_update, now_ms};
use crate::agent::state::{RunLifecycle, RunLifecycleUpdate, ToolCallStates};

pub(super) struct StepCompletion<'a> {
    pub(super) store: &'a crate::state::StateStore,
    pub(super) runtime: &'a PhaseRuntime,
    pub(super) env: &'a ExecutionEnv,
    pub(super) sink: &'a dyn EventSink,
    pub(super) checkpoint_store: Option<&'a dyn ThreadRunStore>,
    pub(super) messages: &'a [Arc<Message>],
    pub(super) input_message_count: usize,
    pub(super) run_identity: &'a RunIdentity,
    pub(super) run_created_at: u64,
    pub(super) total_input_tokens: u64,
    pub(super) total_output_tokens: u64,
    pub(super) thread_ctx: Option<&'a crate::ThreadContextSnapshot>,
}

pub(super) struct CheckpointPersist<'a> {
    pub(super) store: &'a crate::state::StateStore,
    pub(super) checkpoint_store: Option<&'a dyn ThreadRunStore>,
    pub(super) messages: &'a [Arc<Message>],
    pub(super) input_message_count: usize,
    pub(super) run_identity: &'a RunIdentity,
    pub(super) run_created_at: u64,
    pub(super) total_input_tokens: u64,
    pub(super) total_output_tokens: u64,
    pub(super) termination_reason: Option<TerminationReason>,
    pub(super) final_output: Option<String>,
    pub(super) error_payload: Option<Value>,
    pub(super) thread_ctx: Option<&'a crate::ThreadContextSnapshot>,
}

pub(super) async fn complete_step(params: StepCompletion<'_>) -> Result<(), AgentLoopError> {
    let StepCompletion {
        store,
        runtime,
        env,
        sink,
        checkpoint_store,
        messages,
        input_message_count,
        run_identity,
        run_created_at,
        total_input_tokens,
        total_output_tokens,
        thread_ctx,
    } = params;

    commit_update::<RunLifecycle>(
        store,
        RunLifecycleUpdate::StepCompleted {
            updated_at: now_ms(),
        },
    )?;
    let ctx = PhaseContext::new(Phase::StepEnd, store.snapshot())
        .with_run_identity(run_identity.clone())
        .with_messages(messages.to_vec());
    runtime.run_phase_with_context(env, ctx).await?;

    persist_checkpoint(CheckpointPersist {
        store,
        checkpoint_store,
        messages,
        input_message_count,
        run_identity,
        run_created_at,
        total_input_tokens,
        total_output_tokens,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        thread_ctx,
    })
    .await?;

    emit_state_snapshot(store, sink).await;

    sink.emit(AgentEvent::StepEnd).await;
    Ok(())
}

pub(super) async fn persist_checkpoint(
    params: CheckpointPersist<'_>,
) -> Result<(), AgentLoopError> {
    let CheckpointPersist {
        store,
        checkpoint_store,
        messages,
        input_message_count,
        run_identity,
        run_created_at,
        total_input_tokens,
        total_output_tokens,
        termination_reason,
        final_output,
        error_payload,
        thread_ctx,
    } = params;
    let Some(storage) = checkpoint_store else {
        return Ok(());
    };

    let lifecycle = store.read::<RunLifecycle>().unwrap_or_default();
    let state = store
        .export_persisted()
        .map_err(AgentLoopError::PhaseError)?;
    let previous = if let Some(ctx) = thread_ctx {
        ctx.run_cache.get(&run_identity.run_id).cloned()
    } else {
        storage
            .load_run(&run_identity.run_id)
            .await
            .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
    };
    let created_at = previous
        .as_ref()
        .map(|record| record.created_at)
        .unwrap_or(run_created_at / 1000);
    let started_at = previous
        .as_ref()
        .and_then(|record| record.started_at)
        .or(Some(run_created_at / 1000));
    let waiting = waiting_state_from_lifecycle(
        lifecycle.status,
        lifecycle.status_reason.as_deref(),
        run_identity.trace.session_id.clone(),
        waiting_tickets_from_store(store),
    );
    let outcome = termination_reason
        .clone()
        .map(|termination_reason| RunOutcome {
            termination_reason,
            final_output: final_output.clone(),
            error_payload: error_payload.clone(),
        });
    let finished_at = if lifecycle.status.is_terminal() {
        Some(
            if lifecycle.updated_at == 0 {
                run_created_at
            } else {
                lifecycle.updated_at
            } / 1000,
        )
    } else {
        previous.as_ref().and_then(|record| record.finished_at)
    };
    let (msgs, input, output) = materialize_message_log(
        messages,
        previous.as_ref(),
        run_identity,
        lifecycle.step_count,
        input_message_count,
    );
    let record = RunRecord {
        run_id: run_identity.run_id.clone(),
        thread_id: run_identity.thread_id.clone(),
        agent_id: run_identity.agent_id.clone(),
        parent_run_id: run_identity.parent_run_id.clone(),
        request: previous.as_ref().and_then(|record| record.request.clone()),
        input,
        output,
        status: lifecycle.status,
        termination_reason,
        final_output,
        error_payload,
        dispatch_id: run_identity.trace.dispatch_id.clone(),
        session_id: run_identity.trace.session_id.clone(),
        transport_request_id: run_identity.trace.transport_request_id.clone(),
        waiting,
        outcome,
        created_at,
        started_at,
        finished_at,
        updated_at: if lifecycle.updated_at == 0 {
            run_created_at / 1000
        } else {
            lifecycle.updated_at / 1000
        },
        steps: lifecycle.step_count as usize,
        input_tokens: total_input_tokens,
        output_tokens: total_output_tokens,
        state: Some(state),
    };
    storage
        .checkpoint(&run_identity.thread_id, &msgs, &record)
        .await
        .map_err(|e| AgentLoopError::StorageError(e.to_string()))
}

fn materialize_message_log(
    messages: &[Arc<Message>],
    previous: Option<&RunRecord>,
    run_identity: &RunIdentity,
    step_count: u32,
    input_message_count: usize,
) -> (
    Vec<Message>,
    Option<RunMessageInput>,
    Option<RunMessageOutput>,
) {
    let mut msgs: Vec<Message> = messages.iter().map(|message| (**message).clone()).collect();
    let input = previous
        .and_then(|record| record.input.clone())
        .or_else(|| infer_input_from_legacy_request(previous, &run_identity.thread_id, msgs.len()))
        .or_else(|| {
            infer_input_from_initial_messages(&run_identity.thread_id, input_message_count)
        });
    let output_start_seq = input
        .as_ref()
        .and_then(|input| input.range)
        .map(|range| range.to_seq.saturating_add(1))
        .or_else(|| first_existing_produced_seq(&msgs, &run_identity.run_id))
        .unwrap_or(input_message_count as u64 + 1);

    let step_index = step_count.checked_sub(1);
    let mut output_message_ids = previous
        .and_then(|record| record.output.as_ref())
        .map(|output| output.message_ids.clone())
        .unwrap_or_default();
    let mut output_from_seq = previous
        .and_then(|record| record.output.as_ref())
        .and_then(|output| output.range)
        .map(|range| range.from_seq);
    let mut output_to_seq = previous
        .and_then(|record| record.output.as_ref())
        .and_then(|output| output.range)
        .map(|range| range.to_seq);
    let mut has_new_output = false;
    for (index, message) in msgs.iter_mut().enumerate() {
        let seq = index as u64 + 1;
        let existing_output = message.produced_by_run_id() == Some(run_identity.run_id.as_str());
        let new_output = seq >= output_start_seq && is_run_output_message(message);
        if !existing_output && !new_output {
            continue;
        }
        if new_output {
            message.mark_produced_by(&run_identity.run_id, step_index);
            has_new_output = true;
        }
        output_from_seq = Some(output_from_seq.map_or(seq, |from| from.min(seq)));
        output_to_seq = Some(output_to_seq.map_or(seq, |to| to.max(seq)));
        if let Some(id) = message.id.clone()
            && !output_message_ids.iter().any(|existing| existing == &id)
        {
            output_message_ids.push(id);
        }
    }

    let output = if output_from_seq.is_none() || (!has_new_output && output_message_ids.is_empty())
    {
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

    (msgs, input, output)
}

fn infer_input_from_legacy_request(
    previous: Option<&RunRecord>,
    thread_id: &str,
    total_messages: usize,
) -> Option<RunMessageInput> {
    let request = previous.and_then(|record| record.request.as_ref())?;
    let trigger_message_ids = request.input_message_ids.clone();
    let input_count = request
        .input_message_count
        .max(request.input_message_ids.len() as u64);
    if input_count == 0 {
        return None;
    }
    let to_seq = total_messages as u64;
    let from_seq = to_seq.saturating_sub(input_count).saturating_add(1).max(1);
    Some(RunMessageInput {
        thread_id: thread_id.to_string(),
        range: MessageSeqRange::new(from_seq, to_seq),
        trigger_message_ids,
        selected_message_ids: Vec::new(),
        context_policy: None,
        compacted_snapshot_id: None,
    })
}

fn infer_input_from_initial_messages(
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

fn is_run_output_message(message: &Message) -> bool {
    matches!(message.role, Role::Assistant | Role::Tool)
}

fn first_existing_produced_seq(messages: &[Message], run_id: &str) -> Option<u64> {
    messages
        .iter()
        .position(|message| message.produced_by_run_id() == Some(run_id))
        .map(|index| index as u64 + 1)
}

fn waiting_state_from_lifecycle(
    status: RunStatus,
    status_reason: Option<&str>,
    since_dispatch_id: Option<String>,
    tickets: Vec<RunWaitingTicket>,
) -> Option<RunWaitingState> {
    if status != RunStatus::Waiting {
        return None;
    }
    let reason = match status_reason {
        Some("awaiting_tasks") => WaitingReason::BackgroundTasks,
        Some("input_required" | "user_input_required") => WaitingReason::UserInput,
        Some("auth_required" | "suspended") => WaitingReason::ToolPermission,
        Some("rate_limit") => WaitingReason::RateLimit,
        Some("manual_pause") => WaitingReason::ManualPause,
        _ => WaitingReason::ExternalEvent,
    };
    let ticket_ids = tickets
        .iter()
        .map(|ticket| ticket.ticket_id.clone())
        .collect();
    Some(RunWaitingState {
        reason,
        ticket_ids,
        tickets,
        since_dispatch_id,
        message: status_reason.map(ToOwned::to_owned),
    })
}

fn waiting_tickets_from_store(store: &crate::state::StateStore) -> Vec<RunWaitingTicket> {
    let Some(states) = store.read::<ToolCallStates>() else {
        return Vec::new();
    };
    let mut tickets: Vec<RunWaitingTicket> = states
        .calls
        .into_iter()
        .filter(|(_, call)| call.status == ToolCallStatus::Suspended)
        .map(|(call_id, call)| {
            let ticket_id = call
                .suspension_id
                .clone()
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| call_id.clone());
            RunWaitingTicket {
                ticket_id,
                tool_call_id: call_id,
                tool_name: call.tool_name,
                arguments: call.arguments,
                resume_mode: call.resume_mode,
                reason: call.suspension_reason,
                updated_at: call.updated_at,
            }
        })
        .collect();
    tickets.sort_by(|a, b| {
        a.tool_call_id
            .cmp(&b.tool_call_id)
            .then_with(|| a.ticket_id.cmp(&b.ticket_id))
    });
    tickets
}

/// Emit a `StateSnapshot` event with the current persisted state.
pub(super) async fn emit_state_snapshot(store: &crate::state::StateStore, sink: &dyn EventSink) {
    match store.export_persisted() {
        Ok(persisted) => {
            if let Ok(snapshot) = serde_json::to_value(persisted) {
                sink.emit(AgentEvent::StateSnapshot { snapshot }).await;
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to export state snapshot");
        }
    }
}

/// Check if the run lifecycle has left Running state.
///
/// Returns `Some(TerminationReason)` if the run should stop.
pub(super) fn check_termination(store: &crate::state::StateStore) -> Option<TerminationReason> {
    let lifecycle = store.read::<RunLifecycle>()?;
    match lifecycle.status {
        RunStatus::Created => None,
        RunStatus::Running => None,
        RunStatus::Done => {
            let reason = lifecycle.status_reason.as_deref().unwrap_or("unknown");
            Some(TerminationReason::from_done_reason(reason))
        }
        RunStatus::Waiting => match lifecycle.status_reason.as_deref() {
            Some("awaiting_tasks") => None, // orchestrator handles this directly
            _ => Some(TerminationReason::Suspended),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::{ToolCallState, ToolCallStatesUpdate};
    use awaken_contract::contract::suspension::ToolCallResumeMode;
    use serde_json::json;

    fn store_with_loop_state() -> crate::state::StateStore {
        let store = crate::state::StateStore::new();
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .expect("loop state plugin installs");
        store
    }

    #[test]
    fn waiting_state_persists_suspended_tool_tickets() {
        let store = store_with_loop_state();
        commit_update::<ToolCallStates>(
            &store,
            ToolCallStatesUpdate::put(
                ToolCallState::new(
                    "call-1",
                    "dangerous",
                    json!({"path": "/tmp/x"}),
                    ToolCallStatus::Suspended,
                    123,
                )
                .with_resume_mode(ToolCallResumeMode::UseDecisionAsToolResult)
                .with_suspension(Some("ticket-1".into()), Some("approval".into())),
            ),
        )
        .expect("tool state committed");

        let waiting = waiting_state_from_lifecycle(
            RunStatus::Waiting,
            Some("suspended"),
            Some("dispatch-1".into()),
            waiting_tickets_from_store(&store),
        )
        .expect("waiting state");

        assert_eq!(waiting.reason, WaitingReason::ToolPermission);
        assert_eq!(waiting.ticket_ids, vec!["ticket-1"]);
        assert_eq!(waiting.tickets.len(), 1);
        assert_eq!(waiting.tickets[0].tool_call_id, "call-1");
        assert_eq!(waiting.tickets[0].tool_name, "dangerous");
        assert_eq!(waiting.tickets[0].arguments, json!({"path": "/tmp/x"}));
        assert_eq!(
            waiting.tickets[0].resume_mode,
            ToolCallResumeMode::UseDecisionAsToolResult
        );
        assert_eq!(waiting.tickets[0].reason.as_deref(), Some("approval"));
        assert_eq!(waiting.tickets[0].updated_at, 123);
        assert_eq!(waiting.since_dispatch_id.as_deref(), Some("dispatch-1"));
    }

    #[test]
    fn waiting_ticket_falls_back_to_tool_call_id_without_suspension_id() {
        let store = store_with_loop_state();
        commit_update::<ToolCallStates>(
            &store,
            ToolCallStatesUpdate::put(ToolCallState::new(
                "call-without-ticket",
                "plain_suspend",
                json!({"x": 1}),
                ToolCallStatus::Suspended,
                456,
            )),
        )
        .expect("tool state committed");

        let waiting = waiting_state_from_lifecycle(
            RunStatus::Waiting,
            Some("suspended"),
            None,
            waiting_tickets_from_store(&store),
        )
        .expect("waiting state");

        assert_eq!(waiting.ticket_ids, vec!["call-without-ticket"]);
        assert_eq!(waiting.tickets[0].ticket_id, "call-without-ticket");
        assert_eq!(waiting.tickets[0].tool_call_id, "call-without-ticket");
    }

    #[test]
    fn materialize_message_log_preserves_output_across_same_run_resume() {
        let mut old_output = Message::assistant("before wait");
        old_output.id = Some("m-old-output".into());
        old_output.metadata = Some(awaken_contract::contract::message::MessageMetadata {
            run_id: Some("run-1".into()),
            step_index: Some(0),
        });
        let mut new_output = Message::assistant("after resume");
        new_output.id = Some("m-new-output".into());

        let messages = vec![
            Arc::new(Message::user("first input")),
            Arc::new(old_output),
            Arc::new(Message::user("resume input")),
            Arc::new(new_output),
        ];
        let previous = RunRecord {
            run_id: "run-1".into(),
            thread_id: "thread-1".into(),
            agent_id: "agent".into(),
            parent_run_id: None,
            request: None,
            input: Some(RunMessageInput {
                thread_id: "thread-1".into(),
                range: MessageSeqRange::new(1, 3),
                trigger_message_ids: vec!["resume input".into()],
                selected_message_ids: Vec::new(),
                context_policy: None,
                compacted_snapshot_id: None,
            }),
            output: Some(RunMessageOutput {
                thread_id: "thread-1".into(),
                range: MessageSeqRange::new(2, 2),
                message_ids: vec!["m-old-output".into()],
            }),
            status: RunStatus::Waiting,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: 1,
            started_at: None,
            finished_at: None,
            updated_at: 1,
            steps: 1,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        };
        let identity = RunIdentity::new(
            "thread-1".into(),
            None,
            "run-1".into(),
            None,
            "agent".into(),
            awaken_contract::contract::identity::RunOrigin::User,
        );

        let (msgs, _, output) =
            materialize_message_log(&messages, Some(&previous), &identity, 2, 0);

        let output = output.expect("output should be preserved and extended");
        assert_eq!(
            output.message_ids,
            vec!["m-old-output".to_string(), "m-new-output".to_string()]
        );
        assert_eq!(output.range, MessageSeqRange::new(2, 4));
        assert_eq!(msgs[3].produced_by_run_id(), Some("run-1"));
    }
}
