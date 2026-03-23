//! Step completion, checkpointing, state snapshots, and termination checks.

use std::sync::Arc;

use super::super::{ExecutionEnv, PhaseContext, PhaseRuntime};
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunRecord, ThreadRunStore};
use awaken_contract::model::Phase;

use super::{AgentLoopError, commit_update, now_ms};
use crate::agent::state::{RunLifecycle, RunLifecycleUpdate};

pub(super) async fn complete_step(
    store: &crate::state::StateStore,
    runtime: &PhaseRuntime,
    env: &ExecutionEnv,
    sink: &dyn EventSink,
    checkpoint_store: Option<&dyn ThreadRunStore>,
    messages: &[Arc<Message>],
    run_identity: &RunIdentity,
    run_created_at: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
) -> Result<(), AgentLoopError> {
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

    persist_checkpoint(
        store,
        checkpoint_store,
        messages,
        run_identity,
        run_created_at,
        total_input_tokens,
        total_output_tokens,
    )
    .await?;

    emit_state_snapshot(store, sink).await;

    sink.emit(AgentEvent::StepEnd).await;
    Ok(())
}

pub(super) async fn persist_checkpoint(
    store: &crate::state::StateStore,
    checkpoint_store: Option<&dyn ThreadRunStore>,
    messages: &[Arc<Message>],
    run_identity: &RunIdentity,
    run_created_at: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
) -> Result<(), AgentLoopError> {
    let Some(storage) = checkpoint_store else {
        return Ok(());
    };

    let lifecycle = store.read::<RunLifecycle>().unwrap_or_default();
    let state = store
        .export_persisted()
        .map_err(AgentLoopError::PhaseError)?;
    let record = RunRecord {
        run_id: run_identity.run_id.clone(),
        thread_id: run_identity.thread_id.clone(),
        agent_id: run_identity.agent_id.clone(),
        parent_run_id: run_identity.parent_run_id.clone(),
        status: lifecycle.status,
        termination_code: lifecycle.done_reason.clone(),
        created_at: run_created_at / 1000,
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
    let msgs: Vec<Message> = messages.iter().map(|m| (**m).clone()).collect();
    storage
        .checkpoint(&run_identity.thread_id, &msgs, &record)
        .await
        .map_err(|e| AgentLoopError::StorageError(e.to_string()))
}

/// Emit a `StateSnapshot` event with the current persisted state.
pub(super) async fn emit_state_snapshot(store: &crate::state::StateStore, sink: &dyn EventSink) {
    match store.export_persisted() {
        Ok(persisted) => {
            if let Ok(snapshot) = serde_json::to_value(persisted) {
                sink.emit(AgentEvent::StateSnapshot { snapshot }).await;
            }
        }
        Err(_) => {
            // State export failed; skip snapshot emission rather than breaking the loop.
        }
    }
}

/// Check if the run lifecycle has left Running state.
///
/// Returns `Some(TerminationReason)` if the run should stop.
pub(super) fn check_termination(store: &crate::state::StateStore) -> Option<TerminationReason> {
    let lifecycle = store.read::<RunLifecycle>()?;
    match lifecycle.status {
        RunStatus::Running => None,
        RunStatus::Done => {
            let reason = lifecycle.done_reason.as_deref().unwrap_or("unknown");
            Some(TerminationReason::from_done_reason(reason))
        }
        RunStatus::Waiting => Some(TerminationReason::Suspended),
    }
}
