use std::collections::HashSet;

use awaken_runtime_contract::contract::commit_coordinator::{CheckpointCommitPlan, CommitError};
use awaken_runtime_contract::contract::lifecycle::RunStatus;
use awaken_runtime_contract::contract::storage::RunRecord;
use awaken_runtime_contract::now_ms;
use awaken_runtime_contract::state::PersistedState;

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
        resolution_id: previous
            .as_ref()
            .and_then(|record| record.resolution_id.clone()),
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
        const MAX_APPEND_ATTEMPTS: usize = 8;
        for _ in 0..MAX_APPEND_ATTEMPTS {
            let committed_messages = storage
                .load_messages(&root.run_identity.thread_id)
                .await
                .map_err(|error| {
                    ExecutionBackendError::ExecutionFailed(format!(
                        "failed to load thread '{}' before A2A checkpoint append: {error}",
                        root.run_identity.thread_id
                    ))
                })?
                .unwrap_or_default();
            let committed_ids: HashSet<&str> = committed_messages
                .iter()
                .filter_map(|message| message.id.as_deref())
                .collect();
            let delta = root
                .messages
                .iter()
                .filter(|message| {
                    message
                        .id
                        .as_deref()
                        .is_none_or(|id| !committed_ids.contains(id))
                })
                .cloned()
                .collect();
            let plan = CheckpointCommitPlan::append(
                root.run_identity.thread_id.clone(),
                delta,
                Some(committed_messages.len() as u64),
                record.clone(),
            );
            match coordinator.commit_checkpoint(plan).await {
                Ok(_) => return Ok(()),
                Err(CommitError::MessageVersionConflict { .. }) => continue,
                Err(error) => {
                    return Err(ExecutionBackendError::ExecutionFailed(format!(
                        "failed to persist accepted A2A task handle for run '{}': {error}",
                        root.run_identity.run_id
                    )));
                }
            }
        }
        Err(ExecutionBackendError::ExecutionFailed(format!(
            "accepted A2A checkpoint append exhausted {MAX_APPEND_ATTEMPTS} retries under version conflict for thread '{}'",
            root.run_identity.thread_id
        )))
    } else {
        Err(ExecutionBackendError::ExecutionFailed(format!(
            "failed to persist accepted A2A task handle for run '{}': missing CommitCoordinator",
            root.run_identity.run_id
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendControl, BackendRootRunRequest};
    use crate::registry::AgentResolver;
    use awaken_runtime_contract::contract::event_sink::NullEventSink;
    use awaken_runtime_contract::contract::identity::{RunIdentity, RunOrigin};
    use awaken_runtime_contract::contract::message::Message;
    use awaken_runtime_contract::contract::storage::{RunStore, ThreadRunStore, ThreadStore};
    use awaken_stores::{InMemoryStore, MemoryCommitCoordinator};
    use std::collections::HashMap;
    use std::sync::Arc;

    struct NoopResolver;

    impl AgentResolver for NoopResolver {
        fn resolve(
            &self,
            agent_id: &str,
        ) -> Result<crate::registry::ResolvedAgent, crate::RuntimeError> {
            Err(crate::RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
    }

    #[tokio::test]
    async fn accepted_checkpoint_appends_delta_after_concurrent_committed_message() {
        let store = Arc::new(InMemoryStore::new());
        let coordinator = MemoryCommitCoordinator::wrap(Arc::clone(&store));
        let input = Message::user("first").with_id("m-input".into());
        let queued = Message::user("queued while accepted").with_id("m-queued".into());
        let accepted = Message::user("accepted state").with_id("m-accepted".into());
        let previous = RunRecord {
            run_id: "run-1".into(),
            thread_id: "thread-1".into(),
            agent_id: "agent-1".into(),
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

        let resolver = NoopResolver;
        let reader = crate::checkpoint_store::ThreadRunCheckpointStore::new(
            store.clone() as Arc<dyn ThreadRunStore>
        );
        let request = BackendRootRunRequest {
            agent_id: "agent-1",
            messages: vec![input, accepted],
            new_messages: Vec::new(),
            sink: Arc::new(NullEventSink),
            resolver: &resolver,
            run_identity: RunIdentity::new(
                "thread-1".into(),
                None,
                "run-1".into(),
                None,
                "agent-1".into(),
                RunOrigin::User,
            ),
            checkpoint_store: Some(&reader),
            commit: crate::loop_runner::CommitWiring::new(Some(coordinator.as_ref())),
            control: BackendControl::default(),
            decisions: Vec::new(),
            overrides: None,
            frontend_tools: Vec::new(),
            local: None,
            inbox: None,
            is_continuation: false,
        };
        let state = PersistedState {
            revision: 1,
            extensions: HashMap::new(),
        };

        persist_accepted_checkpoint(&A2aExecutionRequest::Root(Box::new(request)), Some(state))
            .await
            .expect("accepted checkpoint persists");

        let committed = store
            .load_messages("thread-1")
            .await
            .expect("load messages")
            .expect("messages exist");
        let ids: Vec<_> = committed
            .iter()
            .map(|message| message.id.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(ids, vec!["m-input", "m-queued", "m-accepted"]);

        let run = store
            .load_run("run-1")
            .await
            .expect("load run")
            .expect("run exists");
        assert_eq!(run.status, RunStatus::Running);
        assert!(run.state.is_some());
    }
}
