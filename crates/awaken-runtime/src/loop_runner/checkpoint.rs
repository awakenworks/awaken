//! Step completion, checkpointing, state snapshots, and termination checks.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::checkpoint_store::RuntimeCheckpointStore;
use crate::hooks::PhaseContext;
use crate::phase::{ExecutionEnv, PhaseRuntime};
use awaken_runtime_contract::contract::commit_coordinator::{
    CommitCoordinator, CommitError, ThreadCommit, ThreadCommitOutcome,
};
use awaken_runtime_contract::contract::event::AgentEvent;
use awaken_runtime_contract::contract::event_sink::EventSink;
use awaken_runtime_contract::contract::identity::RunIdentity;
use awaken_runtime_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_runtime_contract::contract::message::{Message, Role, Visibility};
use awaken_runtime_contract::contract::storage::{
    MessageSeqRange, RunMessageInput, RunMessageOutput, RunOutcome, RunRecord, RunWaitingState,
    RunWaitingTicket, WaitingReason,
};
use awaken_runtime_contract::contract::suspension::ToolCallStatus;
use awaken_runtime_contract::model::Phase;
use serde_json::Value;

use super::{AgentLoopError, commit_update, now_ms};
use crate::agent::state::{RunLifecycle, RunLifecycleUpdate, ToolCallStates};

/// Optional coordinator reference threaded through the loop-runner call
/// chain (ADR-0036). Canonical event drafts are staged and folded into the
/// commit by the (server-supplied) coordinator itself, so the runtime carries
/// no staging buffer here.
#[derive(Default, Clone, Copy)]
pub struct CommitWiring<'a> {
    pub commit_coordinator: Option<&'a dyn CommitCoordinator>,
    pub resolution_id_seed: Option<&'a str>,
}

impl<'a> CommitWiring<'a> {
    /// Wire a coordinator for this run.
    #[must_use]
    pub fn new(coordinator: Option<&'a dyn CommitCoordinator>) -> Self {
        Self {
            commit_coordinator: coordinator,
            resolution_id_seed: None,
        }
    }

    #[must_use]
    pub fn with_resolution_id_seed(mut self, seed: Option<&'a str>) -> Self {
        self.resolution_id_seed = seed;
        self
    }
}

/// Default optimistic-append attempts under version contention (ADR-0038 C6).
pub const MAX_APPEND_ATTEMPTS: usize = 8;

/// Error from the shared version-guarded append-commit loop (ADR-0038 C6a).
#[derive(Debug)]
pub enum CommitAppendError {
    /// Reading the committed log for the version baseline failed.
    Read(String),
    /// The coordinator returned a non-conflict commit error.
    Commit(CommitError),
    /// Exhausted retries under repeated version conflicts.
    Exhausted { thread_id: String, attempts: usize },
}

/// The single version-guarded append-commit entry point shared by every
/// runtime checkpoint path (ADR-0038 C6a: loop-runner, remote-root backend,
/// A2A). It reads the committed log, hands the caller the committed messages
/// plus the expected version so the caller can build its delta + `ThreadCommit`,
/// commits, and retries on `MessageVersionConflict`. Callers supply only the
/// delta/plan builder; the read + version-guard + retry policy lives here.
pub(crate) async fn commit_checkpoint_appending<F>(
    coordinator: &dyn CommitCoordinator,
    storage: &dyn RuntimeCheckpointStore,
    thread_id: &str,
    mut build: F,
) -> Result<ThreadCommitOutcome, CommitAppendError>
where
    F: FnMut(&[Message], u64) -> ThreadCommit,
{
    for _ in 0..MAX_APPEND_ATTEMPTS {
        let committed = storage
            .load_committed_messages(thread_id)
            .await
            .map_err(|error| CommitAppendError::Read(error.to_string()))?
            .unwrap_or_default();
        let expected_version = committed.len() as u64;
        let plan = build(&committed, expected_version);
        match coordinator.commit_checkpoint(plan).await {
            Ok(outcome) => return Ok(outcome),
            Err(CommitError::MessageVersionConflict { .. }) => continue,
            Err(error) => return Err(CommitAppendError::Commit(error)),
        }
    }
    Err(CommitAppendError::Exhausted {
        thread_id: thread_id.to_string(),
        attempts: MAX_APPEND_ATTEMPTS,
    })
}

pub(super) struct StepCompletion<'a> {
    pub(super) store: &'a crate::state::StateStore,
    pub(super) runtime: &'a PhaseRuntime,
    pub(super) env: &'a ExecutionEnv,
    pub(super) sink: &'a dyn EventSink,
    pub(super) checkpoint_store: Option<&'a dyn RuntimeCheckpointStore>,
    pub(super) commit: CommitWiring<'a>,
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
    pub(super) checkpoint_store: Option<&'a dyn RuntimeCheckpointStore>,
    pub(super) commit: CommitWiring<'a>,
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
        commit,
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
        commit,
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
        commit,
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
    if checkpoint_store.is_none() && commit.commit_coordinator.is_none() {
        return Ok(());
    }
    let Some(storage) = checkpoint_store else {
        return Err(AgentLoopError::StorageError(
            "CommitCoordinator requires checkpoint_store for reads".to_string(),
        ));
    };

    let lifecycle = store.read::<RunLifecycle>().unwrap_or_default();
    // Split persisted state by scope (ADR-0038 C4): run-scoped keys ride on
    // the run record, thread-scoped keys land in `ThreadCommit.thread_state_snapshot`
    // and are written in the same commit transaction.
    let state = store
        .export_run_scoped()
        .map_err(AgentLoopError::PhaseError)?;
    let thread_state = store
        .export_thread_scoped()
        .map_err(AgentLoopError::PhaseError)?;
    // The pre-warmed `ThreadContext` only seeds `run_cache` with whatever
    // `latest_run` returned at claim time, which is not necessarily the
    // run we are about to checkpoint (e.g. a second dispatch in a thread
    // where the previous run already completed). Treat the cache as a
    // hot-path optimisation only and fall back to a durable read on miss
    // — otherwise the new record loses carry-forward fields like
    // `activation`, `request`, and `resolution_id`.
    let previous = match thread_ctx.and_then(|ctx| ctx.run_cache.get(&run_identity.run_id).cloned())
    {
        Some(record) => Some(record),
        None => storage
            .load_run(&run_identity.run_id)
            .await
            .map_err(|e| AgentLoopError::StorageError(e.to_string()))?,
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
    let terminal = lifecycle.status.is_terminal();
    let stored_termination_reason = terminal.then(|| termination_reason.clone()).flatten();
    let stored_final_output = terminal.then(|| final_output.clone()).flatten();
    let stored_error_payload = terminal.then(|| error_payload.clone()).flatten();
    let outcome = stored_termination_reason
        .clone()
        .map(|termination_reason| RunOutcome {
            termination_reason,
            final_output: stored_final_output.clone(),
            error_payload: stored_error_payload.clone(),
        });
    let finished_at = if terminal {
        Some(
            if lifecycle.updated_at == 0 {
                run_created_at
            } else {
                lifecycle.updated_at
            } / 1000,
        )
    } else {
        None
    };
    let input = materialize_input(
        messages,
        previous.as_ref(),
        &run_identity.thread_id,
        input_message_count,
    );
    let base_record = RunRecord {
        run_id: run_identity.run_id.clone(),
        thread_id: run_identity.thread_id.clone(),
        agent_id: run_identity.agent_id.clone(),
        parent_run_id: run_identity.parent_run_id.clone(),
        resolution_id: previous
            .as_ref()
            .and_then(|record| record.resolution_id.clone())
            .or_else(|| commit.resolution_id_seed.map(str::to_string)),
        activation: previous
            .as_ref()
            .and_then(|record| record.activation.clone()),
        request: previous.as_ref().and_then(|record| record.request.clone()),
        input,
        output: previous.as_ref().and_then(|record| record.output.clone()),
        status: lifecycle.status,
        termination_reason: stored_termination_reason,
        final_output: stored_final_output,
        error_payload: stored_error_payload,
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
    // ADR-0036 D8: no non-atomic fallback. When `checkpoint_store` is set,
    // a `CommitCoordinator` MUST also be wired — the builder enforces this
    // pairing at `build()` time via `BuildError::CommitCoordinatorRequired`.
    let coordinator = commit.commit_coordinator.ok_or_else(|| {
        AgentLoopError::StorageError(
            "ADR-0036 D8 invariant: checkpoint_store present but no CommitCoordinator wired"
                .to_string(),
        )
    })?;
    let result = commit_checkpoint_appending(
        coordinator,
        storage,
        &run_identity.thread_id,
        |committed_messages, expected_version| {
            let (mut delta, output) = materialize_checkpoint_append(
                messages,
                committed_messages,
                previous.as_ref(),
                run_identity,
                lifecycle.step_count,
                input_message_count,
            );
            // ADR-0038 D11/C7: a freshly-appended compaction summary carries
            // its CompactionMark, resolved here because committed seqs are only
            // known at commit time. The fold (`effective_messages`) reads it.
            stamp_compaction_marks(&mut delta, committed_messages, store);
            let mut record = base_record.clone();
            record.output = output;
            let mut plan = ThreadCommit::append_messages(
                run_identity.thread_id.clone(),
                delta,
                Some(expected_version),
                record,
            );
            // Only attach thread_state when thread-scoped keys exist, so threads
            // without thread-scoped state don't write empty rows on every commit.
            if !thread_state.extensions.is_empty() {
                plan = plan.with_thread_state_snapshot(thread_state.clone());
            }
            plan
        },
    )
    .await;
    result.map(|_| ()).map_err(map_commit_append_error)
}

/// Map the shared commit-append error to the loop runner's error type,
/// preserving the historical version-conflict-exhausted message.
fn map_commit_append_error(error: CommitAppendError) -> AgentLoopError {
    match error {
        CommitAppendError::Read(message) => AgentLoopError::StorageError(message),
        CommitAppendError::Commit(error) => AgentLoopError::StorageError(error.to_string()),
        CommitAppendError::Exhausted {
            thread_id,
            attempts,
        } => AgentLoopError::StorageError(format!(
            "committed append exhausted {attempts} retries under version conflict for thread '{thread_id}'"
        )),
    }
}

/// Is this message a background-compaction summary (the `<conversation-summary>`
/// internal-system message produced by [`crate::context`])?
fn is_compaction_summary(message: &Message) -> bool {
    message.role == Role::System
        && message.visibility == Visibility::Internal
        && message.text().contains("<conversation-summary>")
}

/// Stamp the durable [`CompactionMark`] onto any freshly-appended compaction
/// summary in `delta` (ADR-0038 D11). The covered interval is `[1, to_seq]`
/// where `to_seq` is the committed seq of the latest recorded boundary message
/// (cumulative summaries — scheme A). The committed seq is only knowable here,
/// at commit time, against `committed`. If the boundary message is not in the
/// committed log the summary is left unmarked and the read path falls back to
/// the legacy text-marker trim.
fn stamp_compaction_marks(
    delta: &mut [Message],
    committed: &[Message],
    store: &crate::state::StateStore,
) {
    if !delta.iter().any(is_compaction_summary) {
        return;
    }
    let Some(boundary_id) = store
        .read::<crate::context::plugin::CompactionStateKey>()
        .and_then(|state| {
            state
                .boundaries
                .last()
                .and_then(|boundary| boundary.boundary_message_id.clone())
        })
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let Some(to_seq) = committed
        .iter()
        .position(|message| message.id.as_deref() == Some(boundary_id.as_str()))
        .map(|index| index as u64 + 1)
    else {
        return;
    };
    for message in delta.iter_mut() {
        if is_compaction_summary(message)
            && message
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.compaction)
                .is_none()
        {
            message
                .metadata
                .get_or_insert_with(Default::default)
                .compaction = Some(awaken_runtime_contract::contract::message::CompactionMark {
                from_seq: 1,
                to_seq,
            });
        }
    }
}

fn materialize_input(
    messages: &[Arc<Message>],
    previous: Option<&RunRecord>,
    thread_id: &str,
    input_message_count: usize,
) -> Option<RunMessageInput> {
    previous
        .and_then(|record| record.input.clone())
        .or_else(|| infer_input_from_legacy_request(previous, thread_id, messages.len()))
        .or_else(|| infer_input_from_initial_messages(thread_id, input_message_count))
}

fn materialize_checkpoint_append(
    messages: &[Arc<Message>],
    committed_messages: &[Message],
    previous: Option<&RunRecord>,
    run_identity: &RunIdentity,
    step_count: u32,
    input_message_count: usize,
) -> (Vec<Message>, Option<RunMessageOutput>) {
    let committed_by_id: HashMap<String, (u64, &Message)> = committed_messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            message
                .id
                .as_ref()
                .map(|id| (id.clone(), (index as u64 + 1, message)))
        })
        .collect();
    let previous_output_ids: HashSet<String> = previous
        .and_then(|record| record.output.as_ref())
        .map(|output| output.message_ids.iter().cloned().collect())
        .unwrap_or_default();

    let step_index = step_count.checked_sub(1);
    let mut output_message_ids = previous
        .and_then(|record| record.output.as_ref())
        .map(|output| output.message_ids.clone())
        .unwrap_or_default();
    let mut output_seqs = previous_output_seqs(previous);

    let mut delta = Vec::new();
    let mut next_append_seq = committed_messages.len() as u64 + 1;
    for (index, message) in messages.iter().enumerate() {
        let mut message = (**message).clone();
        let committed = message
            .id
            .as_ref()
            .and_then(|id| committed_by_id.get(id))
            .copied();
        let committed_seq = committed.map(|(seq, _)| seq);
        let seq = committed_seq.unwrap_or(next_append_seq);
        let committed_output = committed.is_some_and(|(_, committed_message)| {
            committed_message.produced_by_run_id() == Some(run_identity.run_id.as_str())
        });
        let previous_output = message
            .id
            .as_ref()
            .is_some_and(|id| previous_output_ids.contains(id));
        let already_recorded_output = message.produced_by_run_id()
            == Some(run_identity.run_id.as_str())
            || committed_output
            || previous_output;
        if already_recorded_output
            && message.produced_by_run_id() != Some(run_identity.run_id.as_str())
        {
            message.metadata = committed
                .and_then(|(_, committed_message)| committed_message.metadata.clone())
                .filter(|metadata| metadata.run_id.as_deref() == Some(run_identity.run_id.as_str()))
                .or(message.metadata);
            message.mark_produced_by(&run_identity.run_id, step_index);
        }
        let new_output = committed_seq.is_none()
            && index >= input_message_count
            && is_run_output_message(&message);

        if already_recorded_output || new_output {
            if new_output {
                message.mark_produced_by(&run_identity.run_id, step_index);
            }
            if let Some(id) = message.id.clone()
                && !output_message_ids.iter().any(|existing| existing == &id)
            {
                output_message_ids.push(id);
                output_seqs.push(seq);
            }
        }

        if committed_seq.is_none() {
            delta.push(message);
            next_append_seq += 1;
        }
    }

    let output = if output_message_ids.is_empty() {
        previous.and_then(|record| record.output.clone())
    } else {
        Some(RunMessageOutput {
            thread_id: run_identity.thread_id.clone(),
            range: contiguous_output_range(output_message_ids.len(), &output_seqs),
            message_ids: output_message_ids,
        })
    };

    (delta, output)
}

#[cfg(test)]
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
    let mut output_seqs = previous_output_seqs(previous);
    for (index, message) in msgs.iter_mut().enumerate() {
        let seq = index as u64 + 1;
        let existing_output = message.produced_by_run_id() == Some(run_identity.run_id.as_str());
        let new_output = seq >= output_start_seq && is_run_output_message(message);
        if !existing_output && !new_output {
            continue;
        }
        if new_output {
            message.mark_produced_by(&run_identity.run_id, step_index);
        }
        if let Some(id) = message.id.clone()
            && !output_message_ids.iter().any(|existing| existing == &id)
        {
            output_message_ids.push(id);
            output_seqs.push(seq);
        }
    }

    let output = if output_message_ids.is_empty() {
        previous.and_then(|record| record.output.clone())
    } else {
        Some(RunMessageOutput {
            thread_id: run_identity.thread_id.clone(),
            range: contiguous_output_range(output_message_ids.len(), &output_seqs),
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

fn previous_output_seqs(previous: Option<&RunRecord>) -> Vec<u64> {
    let Some(output) = previous.and_then(|record| record.output.as_ref()) else {
        return Vec::new();
    };
    let Some(range) = output.range else {
        return Vec::new();
    };
    if range.len() as usize != output.message_ids.len() {
        return Vec::new();
    }
    (range.from_seq..=range.to_seq).collect()
}

fn contiguous_output_range(message_ids_len: usize, seqs: &[u64]) -> Option<MessageSeqRange> {
    if message_ids_len == 0 || seqs.len() != message_ids_len {
        return None;
    }
    let mut seqs = seqs.to_vec();
    seqs.sort_unstable();
    seqs.dedup();
    if seqs.len() != message_ids_len {
        return None;
    }
    let from = *seqs.first()?;
    let to = *seqs.last()?;
    if to.saturating_sub(from) + 1 == message_ids_len as u64 {
        MessageSeqRange::new(from, to)
    } else {
        None
    }
}

fn is_run_output_message(message: &Message) -> bool {
    message.visibility == Visibility::All && matches!(message.role, Role::Assistant | Role::Tool)
}

#[cfg(test)]
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
#[path = "checkpoint_tests.rs"]
mod tests;
