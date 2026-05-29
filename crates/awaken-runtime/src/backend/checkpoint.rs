use std::collections::{HashMap, HashSet};

use awaken_runtime_contract::contract::commit_coordinator::{CheckpointCommitPlan, CommitError};
use awaken_runtime_contract::contract::identity::RunIdentity;
use awaken_runtime_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_runtime_contract::contract::message::{Message, Role};
use awaken_runtime_contract::contract::storage::{
    MessageSeqRange, RunMessageInput, RunMessageOutput, RunOutcome, RunRecord, RunWaitingState,
    WaitingReason,
};
use awaken_runtime_contract::now_ms;
use awaken_runtime_contract::state::PersistedState;
use serde_json::Value;

use crate::checkpoint_store::RuntimeCheckpointStore;
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
    storage: Option<&dyn RuntimeCheckpointStore>,
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
        if commit.commit_coordinator.is_some() {
            return Err(AgentLoopError::StorageError(
                "remote checkpoint requires checkpoint_store when CommitCoordinator is wired"
                    .to_string(),
            ));
        }
        return Ok(());
    };
    validate_remote_checkpoint_identity(thread_id, run_id, run_identity)?;
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
    let input = materialize_remote_input(
        previous.as_ref(),
        &run_identity.thread_id,
        input_message_count,
    );
    let base_record = RunRecord {
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
        output: previous.as_ref().and_then(|record| record.output.clone()),
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
    let canonical_drafts = commit
        .event_buffer
        .map(|buffer| buffer.drain())
        .unwrap_or_default();
    const MAX_APPEND_ATTEMPTS: usize = 8;
    for _ in 0..MAX_APPEND_ATTEMPTS {
        let committed_messages = storage
            .load_committed_messages(thread_id)
            .await
            .map_err(|error| AgentLoopError::StorageError(error.to_string()))?
            .unwrap_or_default();
        let expected_version = committed_messages.len() as u64;
        let (delta, output) = materialize_remote_message_append(
            messages,
            &committed_messages,
            previous.as_ref(),
            run_identity,
            steps,
            input_message_count,
        );
        let mut record = base_record.clone();
        record.output = output;
        let plan = CheckpointCommitPlan::append(
            thread_id.to_string(),
            delta,
            Some(expected_version),
            record,
        )
        .with_canonical_drafts(canonical_drafts.clone());
        match coordinator.commit_checkpoint(plan).await {
            Ok(_) => return Ok(()),
            Err(CommitError::MessageVersionConflict { .. }) => continue,
            Err(error) => return Err(AgentLoopError::StorageError(error.to_string())),
        }
    }
    Err(AgentLoopError::StorageError(format!(
        "remote checkpoint append exhausted {MAX_APPEND_ATTEMPTS} retries under version conflict for thread '{thread_id}'"
    )))
}

fn validate_remote_checkpoint_identity(
    thread_id: &str,
    run_id: &str,
    run_identity: &RunIdentity,
) -> Result<(), AgentLoopError> {
    if thread_id != run_identity.thread_id {
        return Err(AgentLoopError::StorageError(format!(
            "remote checkpoint thread_id '{thread_id}' must match RunIdentity thread_id '{}'",
            run_identity.thread_id
        )));
    }
    if run_id != run_identity.run_id {
        return Err(AgentLoopError::StorageError(format!(
            "remote checkpoint run_id '{run_id}' must match RunIdentity run_id '{}'",
            run_identity.run_id
        )));
    }
    Ok(())
}

fn materialize_remote_input(
    previous: Option<&RunRecord>,
    thread_id: &str,
    input_message_count: usize,
) -> Option<RunMessageInput> {
    previous
        .and_then(|record| record.input.clone())
        .or_else(|| infer_remote_input_from_initial_messages(thread_id, input_message_count))
}

fn materialize_remote_message_append(
    messages: &[Message],
    committed_messages: &[Message],
    previous: Option<&RunRecord>,
    run_identity: &RunIdentity,
    steps: usize,
    input_message_count: usize,
) -> (Vec<Message>, Option<RunMessageOutput>) {
    let committed_seq_by_id: HashMap<String, u64> = committed_messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| message.id.as_ref().map(|id| (id.clone(), index as u64 + 1)))
        .collect();
    let previous_output_ids: HashSet<String> = previous
        .and_then(|record| record.output.as_ref())
        .map(|output| output.message_ids.iter().cloned().collect())
        .unwrap_or_default();
    let step_index = (steps > 0).then_some(steps.saturating_sub(1) as u32);
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
    let mut delta = Vec::new();
    let mut next_append_seq = committed_messages.len() as u64 + 1;
    let mut has_new_output = false;
    for (index, message) in messages.iter().enumerate() {
        let mut message = message.clone();
        let committed_seq = message
            .id
            .as_ref()
            .and_then(|id| committed_seq_by_id.get(id))
            .copied();
        let seq = committed_seq.unwrap_or(next_append_seq);
        let already_recorded_output = message.produced_by_run_id()
            == Some(run_identity.run_id.as_str())
            || message
                .id
                .as_ref()
                .is_some_and(|id| previous_output_ids.contains(id));
        let new_output = committed_seq.is_none()
            && index >= input_message_count
            && is_remote_run_output_message(&message);

        if already_recorded_output || new_output {
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

        if committed_seq.is_none() {
            delta.push(message);
            next_append_seq += 1;
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
    (delta, output)
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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::contract::identity::RunOrigin;
    use awaken_runtime_contract::contract::storage::{RunStore, ThreadRunStore, ThreadStore};
    use awaken_stores::{InMemoryStore, MemoryCommitCoordinator};
    use std::sync::Arc;

    fn identity() -> RunIdentity {
        RunIdentity::new(
            "thread-1".to_string(),
            None,
            "run-1".to_string(),
            None,
            "agent-1".to_string(),
            RunOrigin::User,
        )
    }

    #[test]
    fn remote_checkpoint_identity_accepts_matching_ids() {
        validate_remote_checkpoint_identity("thread-1", "run-1", &identity()).unwrap();
    }

    #[test]
    fn remote_checkpoint_identity_rejects_thread_mismatch() {
        let error =
            validate_remote_checkpoint_identity("other-thread", "run-1", &identity()).unwrap_err();
        assert!(
            matches!(error, AgentLoopError::StorageError(message) if message.contains("thread_id"))
        );
    }

    #[test]
    fn remote_checkpoint_identity_rejects_run_mismatch() {
        let error =
            validate_remote_checkpoint_identity("thread-1", "other-run", &identity()).unwrap_err();
        assert!(
            matches!(error, AgentLoopError::StorageError(message) if message.contains("run_id"))
        );
    }

    #[test]
    fn remote_message_append_preserves_concurrent_committed_messages() {
        let input = Message::user("first").with_id("m-input".into());
        let queued = Message::user("queued while running").with_id("m-queued".into());
        let assistant = Message::assistant("done").with_id("m-assistant".into());
        let previous = RunRecord {
            run_id: "run-1".into(),
            thread_id: "thread-1".into(),
            agent_id: "agent-1".into(),
            input: Some(RunMessageInput {
                thread_id: "thread-1".into(),
                range: MessageSeqRange::new(1, 1),
                trigger_message_ids: vec!["m-input".into()],
                selected_message_ids: Vec::new(),
                context_policy: None,
                compacted_snapshot_id: None,
            }),
            ..Default::default()
        };

        let (delta, output) = materialize_remote_message_append(
            &[input.clone(), assistant],
            &[input, queued],
            Some(&previous),
            &identity(),
            1,
            1,
        );

        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].id.as_deref(), Some("m-assistant"));
        assert_eq!(delta[0].produced_by_run_id(), Some("run-1"));
        let output = output.expect("assistant output is recorded");
        assert_eq!(output.range, MessageSeqRange::new(3, 3));
        assert_eq!(output.message_ids, vec!["m-assistant"]);
    }

    #[tokio::test]
    async fn remote_checkpoint_appends_delta_after_concurrent_committed_message() {
        let store = Arc::new(InMemoryStore::new());
        let coordinator = MemoryCommitCoordinator::wrap(Arc::clone(&store));
        let input = Message::user("first").with_id("m-input".into());
        let queued = Message::user("queued while running").with_id("m-queued".into());
        let assistant = Message::assistant("done").with_id("m-assistant".into());
        let previous = RunRecord {
            run_id: "run-1".into(),
            thread_id: "thread-1".into(),
            agent_id: "agent-1".into(),
            input: Some(RunMessageInput {
                thread_id: "thread-1".into(),
                range: MessageSeqRange::new(1, 1),
                trigger_message_ids: vec!["m-input".into()],
                selected_message_ids: Vec::new(),
                context_policy: None,
                compacted_snapshot_id: None,
            }),
            status: RunStatus::Created,
            ..Default::default()
        };
        store
            .checkpoint_append("thread-1", std::slice::from_ref(&input), Some(0), &previous)
            .await
            .expect("seed input");
        store
            .checkpoint_append(
                "thread-1",
                std::slice::from_ref(&queued),
                Some(1),
                &RunRecord {
                    run_id: "run-queued".into(),
                    thread_id: "thread-1".into(),
                    agent_id: "agent-1".into(),
                    status: RunStatus::Created,
                    ..Default::default()
                },
            )
            .await
            .expect("concurrent append");

        let checkpoint_reader = crate::checkpoint_store::ThreadRunCheckpointStore::new(
            store.clone() as Arc<dyn ThreadRunStore>,
        );
        persist_remote_root_checkpoint(
            Some(&checkpoint_reader),
            "thread-1",
            "run-1",
            "agent-1",
            None,
            1_000,
            &[input, assistant],
            1,
            RunStatus::Done,
            None,
            None,
            Some("done".to_string()),
            None,
            &identity(),
            1,
            None,
            crate::loop_runner::CommitWiring::new(Some(coordinator.as_ref()), None),
        )
        .await
        .expect("remote checkpoint persists");

        let committed = store
            .load_messages("thread-1")
            .await
            .expect("load messages")
            .expect("messages exist");
        let ids: Vec<_> = committed
            .iter()
            .map(|message| message.id.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(ids, vec!["m-input", "m-queued", "m-assistant"]);
        assert_eq!(committed[2].produced_by_run_id(), Some("run-1"));

        let run = store
            .load_run("run-1")
            .await
            .expect("load run")
            .expect("run exists");
        let output = run.output.expect("output persisted");
        assert_eq!(output.range, MessageSeqRange::new(3, 3));
        assert_eq!(output.message_ids, vec!["m-assistant"]);
    }
}
