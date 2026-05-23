use awaken_contract::contract::commit_coordinator::CheckpointCommitPlan;
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::storage::RunRecord;
use awaken_contract::now_ms;
use awaken_contract::state::PersistedState;

use crate::backend::ExecutionBackendError;

use super::A2aExecutionRequest;

pub(super) async fn persist_accepted_checkpoint(
    request: &A2aExecutionRequest<'_>,
    state: Option<PersistedState>,
) -> Result<(), ExecutionBackendError> {
    let (root, storage, state) = match request {
        A2aExecutionRequest::Root(root) => {
            let Some(storage) = root.checkpoint_store else {
                return Ok(());
            };
            let Some(state) = state else {
                return Ok(());
            };
            (root, storage, state)
        }
        A2aExecutionRequest::Delegate(_) => return Ok(()),
    };
    let now = now_ms() / 1000;
    let previous = storage
        .load_run(&root.run_identity.run_id)
        .await
        .map_err(|error| {
            ExecutionBackendError::ExecutionFailed(format!(
                "failed to load run '{}' before A2A checkpoint: {error}",
                root.run_identity.run_id
            ))
        })?;
    let record = RunRecord {
        run_id: root.run_identity.run_id.clone(),
        thread_id: root.run_identity.thread_id.clone(),
        agent_id: root.agent_id.to_string(),
        parent_run_id: root.run_identity.parent_run_id.clone(),
        registry_manifest: previous
            .as_ref()
            .and_then(|record| record.registry_manifest.clone()),
        activation: previous
            .as_ref()
            .and_then(|record| record.activation.clone()),
        request: previous.as_ref().and_then(|record| record.request.clone()),
        input: previous.as_ref().and_then(|record| record.input.clone()),
        output: previous.as_ref().and_then(|record| record.output.clone()),
        status: RunStatus::Running,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: root.run_identity.trace.dispatch_id.clone(),
        session_id: root.run_identity.trace.session_id.clone(),
        transport_request_id: root.run_identity.trace.transport_request_id.clone(),
        waiting: None,
        outcome: None,
        created_at: previous
            .as_ref()
            .map(|record| record.created_at)
            .unwrap_or(now),
        started_at: previous
            .as_ref()
            .and_then(|record| record.started_at)
            .or(Some(now)),
        finished_at: None,
        updated_at: now,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: Some(state),
    };
    if let Some(coordinator) = root.commit.commit_coordinator {
        let plan = CheckpointCommitPlan::checkpoint_only(
            root.run_identity.thread_id.clone(),
            root.messages.clone(),
            record,
        );
        coordinator.commit_checkpoint(plan).await.map_err(|error| {
            ExecutionBackendError::ExecutionFailed(format!(
                "failed to persist accepted A2A task handle for run '{}': {error}",
                root.run_identity.run_id
            ))
        })?;
        Ok(())
    } else {
        Err(ExecutionBackendError::ExecutionFailed(format!(
            "failed to persist accepted A2A task handle for run '{}': missing CommitCoordinator",
            root.run_identity.run_id
        )))
    }
}
