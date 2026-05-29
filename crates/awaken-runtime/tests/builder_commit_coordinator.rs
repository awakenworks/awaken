//! Integration test for `AgentRuntimeBuilder::with_commit_coordinator` (ADR-0036).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime_contract::contract::commit_coordinator::{
    CheckpointCommitOutcome, CheckpointCommitPlan, CommitCoordinator, CommitError,
    TransactionScopeId,
};
use awaken_runtime_contract::contract::storage::ThreadRunStore;
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

/// Per-run coordinator override plumbing: `RunActivation`'s
/// `with_commit_coordinator_override` attaches the server staging coordinator
/// for one run, which the runtime prefers over its build-time coordinator. The
/// runtime never observes a canonical-event staging buffer.
#[test]
fn run_activation_accepts_commit_coordinator_override() {
    use awaken_runtime::RunActivation;

    let coordinator: Arc<dyn CommitCoordinator> = Arc::new(NoopCoord::default());
    let activation = RunActivation::new("t-override", Vec::new())
        .with_commit_coordinator_override(Arc::clone(&coordinator));

    let attached = activation
        .control
        .commit_coordinator_override
        .as_ref()
        .expect("override must be attached to RunActivation");
    assert!(Arc::ptr_eq(attached, &coordinator));

    // A neutral activation carries no override.
    let neutral = RunActivation::new("t-neutral", Vec::new());
    assert!(neutral.control.commit_coordinator_override.is_none());
}
