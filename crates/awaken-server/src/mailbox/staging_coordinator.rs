//! Per-run commit coordinator that folds dispatch-staged canonical event
//! drafts into each checkpoint commit.
//!
//! The dispatch path mints one [`EventBuffer`] per run and shares it with the
//! `DurableEventSink` (which stages canonical drafts as runtime events flow)
//! and with this coordinator (which drains them at commit time). Wrapping the
//! mailbox's base [`CommitCoordinator`] this way keeps the staging buffer off
//! the runtime entirely: the runtime builds a plain `CheckpointCommitPlan`
//! and never observes the canonical drafts, while atomicity (drafts + run
//! record commit together) is preserved here.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime::EventBuffer;
use awaken_server_contract::contract::commit_coordinator::{
    CheckpointCommitOutcome, CheckpointCommitPlan, CommitCoordinator, CommitError,
    StagedCanonicalEvent, TransactionScopeId,
};
use awaken_server_contract::contract::storage::RuntimeCheckpointStore;
use parking_lot::Mutex;

/// Wraps a base [`CommitCoordinator`], draining a per-run [`EventBuffer`] into
/// each checkpoint commit.
pub(super) struct StagingCommitCoordinator {
    inner: Arc<dyn CommitCoordinator>,
    buffer: Arc<EventBuffer>,
    /// Drafts staged but not yet committed. They survive the runtime's
    /// version-conflict retry loop (each retry re-submits the same plan) and
    /// are cleared only once a commit succeeds.
    pending: Mutex<Vec<StagedCanonicalEvent>>,
}

impl StagingCommitCoordinator {
    pub(super) fn new(inner: Arc<dyn CommitCoordinator>, buffer: Arc<EventBuffer>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            buffer,
            pending: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl CommitCoordinator for StagingCommitCoordinator {
    fn scope(&self) -> TransactionScopeId {
        self.inner.scope()
    }

    fn reader(&self) -> Arc<dyn RuntimeCheckpointStore> {
        self.inner.reader()
    }

    async fn commit_checkpoint(
        &self,
        mut plan: CheckpointCommitPlan,
    ) -> Result<CheckpointCommitOutcome, CommitError> {
        // Accumulate newly staged drafts onto any that an earlier (conflicted)
        // attempt staged, so a version-conflict retry never drops events.
        let drafts = {
            let mut pending = self.pending.lock();
            pending.extend(self.buffer.drain());
            pending.clone()
        };
        plan.canonical_drafts = drafts;
        let outcome = self.inner.commit_checkpoint(plan).await?;
        // Success: the drafts are durable, so the pending buffer is cleared.
        // On error we leave `pending` intact for the runtime's retry.
        self.pending.lock().clear();
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_server_contract::contract::commit_coordinator::CanonicalEventStager;
    use awaken_server_contract::contract::event_store::{
        CanonicalEventDraft, CanonicalEventKind, EventScope, EventVisibility,
    };
    use awaken_server_contract::contract::storage::RunRecord;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn sample_draft(kind: &str) -> CanonicalEventDraft {
        let mut draft = CanonicalEventDraft::new(
            vec![EventScope::thread("t-1"), EventScope::run("run-1")],
            CanonicalEventKind::new(kind).unwrap(),
            json!({ "kind": kind }),
            "test",
        )
        .unwrap();
        draft.visibility = EventVisibility::Public;
        draft
    }

    fn run_record() -> RunRecord {
        RunRecord {
            run_id: "run-1".into(),
            thread_id: "t-1".into(),
            agent_id: "agent-1".into(),
            ..Default::default()
        }
    }

    /// Records the `canonical_drafts` it observed for each commit and can be
    /// scripted to fail the first N commits (version conflict) to exercise
    /// retry-safe draining.
    struct RecordingCoordinator {
        observed: Mutex<Vec<Vec<String>>>,
        fail_first: AtomicU32,
    }

    impl RecordingCoordinator {
        fn new(fail_first: u32) -> Arc<Self> {
            Arc::new(Self {
                observed: Mutex::new(Vec::new()),
                fail_first: AtomicU32::new(fail_first),
            })
        }
    }

    #[async_trait]
    impl CommitCoordinator for RecordingCoordinator {
        fn scope(&self) -> TransactionScopeId {
            TransactionScopeId::new("recording").unwrap()
        }
        fn reader(&self) -> Arc<dyn RuntimeCheckpointStore> {
            unreachable!("test does not read the store")
        }
        async fn commit_checkpoint(
            &self,
            plan: CheckpointCommitPlan,
        ) -> Result<CheckpointCommitOutcome, CommitError> {
            let kinds: Vec<String> = plan
                .canonical_drafts
                .iter()
                .map(|staged| staged.draft.event_kind.as_str().to_string())
                .collect();
            self.observed.lock().push(kinds);
            if self.fail_first.load(Ordering::SeqCst) > 0 {
                self.fail_first.fetch_sub(1, Ordering::SeqCst);
                return Err(CommitError::MessageVersionConflict {
                    thread_id: plan.thread_id.clone(),
                    expected: 0,
                    actual: 1,
                });
            }
            Ok(CheckpointCommitOutcome::default())
        }
    }

    #[tokio::test]
    async fn drains_buffer_into_plan_on_commit() {
        let buffer = Arc::new(EventBuffer::new());
        buffer.stage(sample_draft("RunStarted"));
        buffer.stage(sample_draft("RunCompleted"));
        let inner = RecordingCoordinator::new(0);
        let staging = StagingCommitCoordinator::new(inner.clone(), buffer.clone());

        let plan = CheckpointCommitPlan::checkpoint_only("t-1", vec![], run_record());
        staging.commit_checkpoint(plan).await.unwrap();

        let observed = inner.observed.lock();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0], vec!["RunStarted", "RunCompleted"]);
        assert!(buffer.is_empty(), "buffer drained after commit");
    }

    #[tokio::test]
    async fn retry_after_conflict_resubmits_same_drafts() {
        let buffer = Arc::new(EventBuffer::new());
        buffer.stage(sample_draft("RunStarted"));
        // Fail the first commit (version conflict); the runtime retries by
        // calling commit_checkpoint again. The staged draft must reappear.
        let inner = RecordingCoordinator::new(1);
        let staging = StagingCommitCoordinator::new(inner.clone(), buffer.clone());

        let conflict = staging
            .commit_checkpoint(CheckpointCommitPlan::checkpoint_only(
                "t-1",
                vec![],
                run_record(),
            ))
            .await;
        assert!(matches!(
            conflict,
            Err(CommitError::MessageVersionConflict { .. })
        ));
        // Retry: buffer is already empty, but the draft was retained.
        staging
            .commit_checkpoint(CheckpointCommitPlan::checkpoint_only(
                "t-1",
                vec![],
                run_record(),
            ))
            .await
            .unwrap();

        let observed = inner.observed.lock();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0], vec!["RunStarted"]);
        assert_eq!(
            observed[1],
            vec!["RunStarted"],
            "conflicted draft must survive retry"
        );
    }

    #[tokio::test]
    async fn cleared_after_success_does_not_leak_into_next_commit() {
        let buffer = Arc::new(EventBuffer::new());
        buffer.stage(sample_draft("RunStarted"));
        let inner = RecordingCoordinator::new(0);
        let staging = StagingCommitCoordinator::new(inner.clone(), buffer.clone());

        staging
            .commit_checkpoint(CheckpointCommitPlan::checkpoint_only(
                "t-1",
                vec![],
                run_record(),
            ))
            .await
            .unwrap();
        // Second checkpoint with a fresh draft must not re-include the first.
        buffer.stage(sample_draft("StepEnd"));
        staging
            .commit_checkpoint(CheckpointCommitPlan::checkpoint_only(
                "t-1",
                vec![],
                run_record(),
            ))
            .await
            .unwrap();

        let observed = inner.observed.lock();
        assert_eq!(observed[0], vec!["RunStarted"]);
        assert_eq!(observed[1], vec!["StepEnd"]);
    }
}
