//! Integration test for `AgentRuntimeBuilder::with_commit_coordinator` (ADR-0036).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::commit_coordinator::{
    CheckpointCommitOutcome, CheckpointCommitPlan, CommitCoordinator, CommitError,
    TransactionScopeId,
};
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_stores::InMemoryStore;

struct NoopCoord {
    thread_run: Arc<InMemoryStore>,
}

impl Default for NoopCoord {
    fn default() -> Self {
        Self {
            thread_run: Arc::new(InMemoryStore::new()),
        }
    }
}

#[async_trait]
impl CommitCoordinator for NoopCoord {
    fn scope(&self) -> TransactionScopeId {
        TransactionScopeId::new("test::noop").unwrap()
    }

    fn thread_run_store(&self) -> Arc<dyn ThreadRunStore> {
        Arc::clone(&self.thread_run) as Arc<dyn ThreadRunStore>
    }

    async fn commit_checkpoint(
        &self,
        _plan: CheckpointCommitPlan,
    ) -> Result<CheckpointCommitOutcome, CommitError> {
        Ok(CheckpointCommitOutcome::default())
    }
}

#[test]
fn builder_wires_commit_coordinator_into_runtime() {
    let runtime = AgentRuntimeBuilder::new()
        .with_commit_coordinator(Arc::new(NoopCoord::default()))
        .build()
        .unwrap();
    let coord = runtime
        .commit_coordinator()
        .expect("coordinator should be wired");
    assert_eq!(coord.scope().as_str(), "test::noop");
}

/// Event buffer plumbing: `RunActivation::with_event_buffer` must accept the
/// same `Arc<EventBuffer>` the sink-wrap layer receives so the staging buffer
/// and the checkpoint-drain buffer share identity.
#[test]
fn run_request_accepts_shared_event_buffer() {
    use awaken_contract::contract::commit_coordinator::CanonicalEventStager;
    use awaken_contract::contract::event_store::{
        CanonicalEventDraft, CanonicalEventKind, EventScope,
    };
    use awaken_runtime::EventBuffer;
    use awaken_runtime::RunActivation;
    use serde_json::json;

    let buffer = Arc::new(EventBuffer::new());
    let request = RunActivation::new("t-buf", Vec::new()).with_event_buffer(Arc::clone(&buffer));

    // The buffer field is exposed for the sink-wrap layer to pick up.
    let from_request = request
        .capture
        .event_buffer
        .as_ref()
        .expect("buffer must be attached to RunActivation");
    assert!(Arc::ptr_eq(from_request, &buffer));

    // Staging through the same handle is observable from the original Arc,
    // proving sink-side stages and runtime-side drains can share state.
    let draft = CanonicalEventDraft::new(
        vec![EventScope::thread("t-buf"), EventScope::run("r-buf")],
        CanonicalEventKind::new("ToolCallReady").unwrap(),
        json!({"id": "c1"}),
        "test",
    )
    .unwrap();
    (Arc::clone(&buffer) as Arc<dyn CanonicalEventStager>).stage(draft);
    assert_eq!(buffer.len(), 1, "stage must reach the shared buffer");
}
