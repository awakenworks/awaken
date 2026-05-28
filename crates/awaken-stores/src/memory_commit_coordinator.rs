//! In-memory [`CommitCoordinator`] implementation (ADR-0036).
//!
//! Serialises checkpoint commits behind a single async mutex and simulates
//! transactional atomicity by snapshotting each backing store before the
//! write batch and restoring those snapshots on any failure.
//!
//! Limitations of the in-memory rollback model are intentional and
//! documented:
//!
//! - The canonical event broadcast channel is fire-and-forget. Subscribers
//!   that observed an event whose commit was later rolled back still see
//!   it on the live tail. Replay (cursor-based read) reflects the rolled
//!   back state correctly.
//! - Concurrent reads observe the pre-commit snapshot until the commit
//!   succeeds, because the outer mutex serialises commits but the store
//!   writes themselves run sequentially under their own per-store locks.
//!   This is consistent with "checkpoint-batched atomicity" per ADR-0036
//!   D1 (a partial batch is never observable to a subsequent commit).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_server_contract::contract::commit_coordinator::{
    CheckpointCommitOutcome, CheckpointCommitPlan, CommitCoordinator, CommitError,
    MessageWriteMode, TransactionScopeId,
};
use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::message::PendingMessageRecord;
use awaken_server_contract::contract::storage::{RunRecord, ThreadRunStore};
use awaken_server_contract::thread::Thread;
use tokio::sync::Mutex;

use crate::commit_batch::run_commit_batch;
use crate::memory::InMemoryStore;
use crate::memory_event_store::InMemoryEventStore;
use crate::memory_outbox::InMemoryOutboxStore;

/// Snapshot of [`InMemoryStore`] thread/run/message state used for
/// rollback on commit failure.
#[derive(Debug, Clone)]
struct ThreadRunSnapshot {
    threads: HashMap<String, Thread>,
    runs: HashMap<String, RunRecord>,
    messages: HashMap<String, Vec<Message>>,
    pending_messages: HashMap<String, Vec<PendingMessageRecord>>,
}

async fn snapshot_thread_run_state(store: &InMemoryStore) -> ThreadRunSnapshot {
    ThreadRunSnapshot {
        threads: store.threads.read().await.clone(),
        runs: store.runs.read().await.clone(),
        messages: store.messages.read().await.clone(),
        pending_messages: store.pending_messages.read().await.clone(),
    }
}

async fn restore_thread_run_state(store: &InMemoryStore, snapshot: ThreadRunSnapshot) {
    *store.threads.write().await = snapshot.threads;
    *store.runs.write().await = snapshot.runs;
    *store.messages.write().await = snapshot.messages;
    *store.pending_messages.write().await = snapshot.pending_messages;
}

/// Coordinator that drives [`InMemoryStore`], [`InMemoryEventStore`], and
/// [`InMemoryOutboxStore`] through a simulated transaction.
#[derive(Debug, Clone)]
pub struct MemoryCommitCoordinator {
    thread_run: Arc<InMemoryStore>,
    events: Arc<InMemoryEventStore>,
    outbox: Arc<InMemoryOutboxStore>,
    /// Coordinator-level mutex serialising commit batches.
    commit_lock: Arc<Mutex<()>>,
    /// Stable scope id derived from the store identities.
    scope: TransactionScopeId,
}

impl MemoryCommitCoordinator {
    /// Construct a coordinator wrapping the three in-memory stores. The
    /// caller is responsible for using these same `Arc` handles for all
    /// store consumers in the deployment so that scope is consistent.
    pub fn new(
        thread_run: Arc<InMemoryStore>,
        events: Arc<InMemoryEventStore>,
        outbox: Arc<InMemoryOutboxStore>,
    ) -> Result<Self, CommitError> {
        let scope_descriptor = format!(
            "memory::({:p},{:p},{:p})",
            Arc::as_ptr(&thread_run),
            Arc::as_ptr(&events),
            Arc::as_ptr(&outbox)
        );
        let scope = TransactionScopeId::new(scope_descriptor)?;
        Ok(Self {
            thread_run,
            events,
            outbox,
            commit_lock: Arc::new(Mutex::new(())),
            scope,
        })
    }

    /// Convenience constructor that creates fresh in-memory stores and
    /// returns the coordinator together with its thread-run store handle.
    /// Intended for tests and examples where the three backing stores do
    /// not need to be separately referenced.
    pub fn default_in_memory() -> (Arc<Self>, Arc<InMemoryStore>) {
        let store = Arc::new(InMemoryStore::new());
        let coord = Self::wrap(Arc::clone(&store));
        (coord, store)
    }

    /// Wrap an existing [`InMemoryStore`] with fresh event/outbox stores.
    /// Convenience for tests that already hold a shared thread-run store
    /// and need to attach the coordinator surface on top.
    pub fn wrap(store: Arc<InMemoryStore>) -> Arc<Self> {
        let events = Arc::new(InMemoryEventStore::new());
        let outbox = Arc::new(InMemoryOutboxStore::new());
        Arc::new(Self::new(store, events, outbox).expect("in-memory coordinator constructs"))
    }
}

#[async_trait]
impl CommitCoordinator for MemoryCommitCoordinator {
    fn scope(&self) -> TransactionScopeId {
        self.scope.clone()
    }

    fn thread_run_store(
        &self,
    ) -> Arc<dyn awaken_server_contract::contract::storage::ThreadRunStore> {
        Arc::clone(&self.thread_run)
            as Arc<dyn awaken_server_contract::contract::storage::ThreadRunStore>
    }

    async fn commit_checkpoint(
        &self,
        plan: CheckpointCommitPlan,
    ) -> Result<CheckpointCommitOutcome, CommitError> {
        plan.validate()?;

        // Serialise commits so concurrent attempts do not interleave
        // partial state.
        let _guard = self.commit_lock.lock().await;

        // The shared batch helper rolls back event/outbox state on failure.
        // The thread-run store is rolled back here through a snapshot taken
        // before the checkpoint write — only the in-memory backend can
        // reverse the in-memory map writes.
        let thread_run_snapshot = snapshot_thread_run_state(&self.thread_run).await;
        let thread_run = self.thread_run.clone();
        let plan_ref = &plan;
        let write_thread_run = || async move {
            let result = match plan_ref.message_mode {
                MessageWriteMode::Overwrite =>
                {
                    #[allow(deprecated)]
                    thread_run
                        .checkpoint(&plan_ref.thread_id, &plan_ref.messages, &plan_ref.run)
                        .await
                }
                MessageWriteMode::Append => thread_run
                    .checkpoint_append(
                        &plan_ref.thread_id,
                        &plan_ref.messages,
                        plan_ref.expected_message_version,
                        &plan_ref.run,
                    )
                    .await
                    .map(|_| ()),
            };
            match result {
                Ok(()) => Ok(()),
                Err(error) => {
                    restore_thread_run_state(&thread_run, thread_run_snapshot).await;
                    Err(error)
                }
            }
        };

        let outcome = run_commit_batch(&plan, &self.events, &self.outbox, write_thread_run).await;
        match plan.message_mode {
            MessageWriteMode::Append => {
                outcome.map_err(|error| error.reclassify_append_conflict(&plan.thread_id))
            }
            MessageWriteMode::Overwrite => outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_server_contract::contract::commit_coordinator::StagedCanonicalEvent;
    use awaken_server_contract::contract::event_store::{
        AppendOptions, CanonicalEventDraft, CanonicalEventKind, EventReader, EventScope,
        EventVisibility, EventWriter,
    };
    use awaken_server_contract::contract::lifecycle::RunStatus;
    use awaken_server_contract::contract::storage::{RunRecord, ThreadStore};
    use serde_json::json;

    fn run_record(thread_id: &str, run_id: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            thread_id: thread_id.to_string(),
            status: RunStatus::Done,
            agent_id: "agent-1".to_string(),
            ..Default::default()
        }
    }

    fn sample_draft(kind: &str, thread_id: &str, run_id: &str) -> CanonicalEventDraft {
        let mut draft = CanonicalEventDraft::new(
            vec![EventScope::thread(thread_id), EventScope::run(run_id)],
            CanonicalEventKind::new(kind).unwrap(),
            json!({"kind": kind}),
            "test",
        )
        .unwrap();
        draft.visibility = EventVisibility::Public;
        draft
    }

    fn build_coordinator() -> (
        MemoryCommitCoordinator,
        Arc<InMemoryStore>,
        Arc<InMemoryEventStore>,
        Arc<InMemoryOutboxStore>,
    ) {
        let thread_run = Arc::new(InMemoryStore::new());
        let events = Arc::new(InMemoryEventStore::new());
        let outbox = Arc::new(InMemoryOutboxStore::new());
        let coord =
            MemoryCommitCoordinator::new(thread_run.clone(), events.clone(), outbox.clone())
                .unwrap();
        (coord, thread_run, events, outbox)
    }

    #[tokio::test]
    async fn commit_empty_plan_persists_checkpoint() {
        let (coord, thread_run, events, _outbox) = build_coordinator();
        let plan =
            CheckpointCommitPlan::checkpoint_only("t-1", Vec::new(), run_record("t-1", "r-1"));

        let outcome = coord.commit_checkpoint(plan).await.unwrap();
        assert!(outcome.canonical_event_ids.is_empty());
        assert!(outcome.additional_outbox_ids.is_empty());

        // Thread persisted.
        let loaded = thread_run.load_thread("t-1").await.unwrap();
        assert!(loaded.is_some());
        // No events appended.
        let count = events.count(EventScope::run("r-1")).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn commit_with_drafts_appends_and_persists() {
        let (coord, thread_run, events, _outbox) = build_coordinator();
        let plan =
            CheckpointCommitPlan::checkpoint_only("t-2", Vec::new(), run_record("t-2", "r-2"))
                .with_canonical_drafts(vec![
                    StagedCanonicalEvent::new(sample_draft("RunStarted", "t-2", "r-2")),
                    StagedCanonicalEvent::new(sample_draft("RunCompleted", "t-2", "r-2")),
                ]);

        let outcome = coord.commit_checkpoint(plan).await.unwrap();
        assert_eq!(outcome.canonical_event_ids.len(), 2);

        let thread = thread_run.load_thread("t-2").await.unwrap();
        assert!(thread.is_some());
        let count = events.count(EventScope::run("r-2")).await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn commit_append_plan_advances_committed_messages() {
        use awaken_server_contract::contract::message::Message;
        let (coord, thread_run, _events, _outbox) = build_coordinator();

        let plan = CheckpointCommitPlan::append(
            "t-ap",
            vec![Message::user("a")],
            Some(0),
            run_record("t-ap", "r-1"),
        );
        coord.commit_checkpoint(plan).await.unwrap();

        let plan = CheckpointCommitPlan::append(
            "t-ap",
            vec![Message::user("b"), Message::user("c")],
            Some(1),
            run_record("t-ap", "r-2"),
        );
        coord.commit_checkpoint(plan).await.unwrap();

        let msgs = thread_run.load_messages("t-ap").await.unwrap().unwrap();
        assert_eq!(msgs.len(), 3, "append plans accumulate, not overwrite");
    }

    #[tokio::test]
    async fn commit_append_plan_stale_version_yields_message_conflict() {
        use awaken_server_contract::contract::message::Message;
        let (coord, thread_run, _events, _outbox) = build_coordinator();

        let plan = CheckpointCommitPlan::append(
            "t-c",
            vec![Message::user("a"), Message::user("b")],
            Some(0),
            run_record("t-c", "r-1"),
        );
        coord.commit_checkpoint(plan).await.unwrap();

        // Committed length is 2, so expecting 0 must reclassify to a
        // message-level conflict carrying the thread id.
        let plan = CheckpointCommitPlan::append(
            "t-c",
            vec![Message::user("c")],
            Some(0),
            run_record("t-c", "r-2"),
        );
        let err = coord.commit_checkpoint(plan).await.unwrap_err();
        match err {
            CommitError::MessageVersionConflict {
                thread_id,
                expected,
                actual,
            } => {
                assert_eq!(thread_id, "t-c");
                assert_eq!(expected, 0);
                assert_eq!(actual, 2);
            }
            other => panic!("expected MessageVersionConflict, got {other:?}"),
        }
        let msgs = thread_run.load_messages("t-c").await.unwrap().unwrap();
        assert_eq!(msgs.len(), 2, "a conflicting append leaves state untouched");
    }

    #[tokio::test]
    async fn commit_rolls_back_on_event_append_idempotency_conflict() {
        let (coord, thread_run, events, _outbox) = build_coordinator();

        // Seed an idempotent event in the canonical store directly so the
        // next commit's append collides with a different digest under the
        // same idempotency identity.
        let seeded_draft = sample_draft("RunStarted", "t-3", "r-3");
        let seed_options = AppendOptions {
            writer_id: Some("runtime".to_string()),
            idempotency_key: Some("k-collide".to_string()),
            ..Default::default()
        };
        events
            .append(seeded_draft, seed_options.clone())
            .await
            .unwrap();

        // Different payload under the same idempotency identity → conflict.
        let mut conflicting_draft = sample_draft("RunStarted", "t-3", "r-3");
        conflicting_draft.payload = json!({"kind": "RunStarted", "different": true});

        let plan =
            CheckpointCommitPlan::checkpoint_only("t-3", Vec::new(), run_record("t-3", "r-3"))
                .with_canonical_drafts(vec![
                    StagedCanonicalEvent::new(conflicting_draft).with_options(seed_options),
                ]);

        // Capture pre-commit event count for rollback assertion.
        let pre_count = events.count(EventScope::run("r-3")).await.unwrap();

        let result = coord.commit_checkpoint(plan).await;
        assert!(matches!(result, Err(CommitError::EventAppend(_))));

        // Thread checkpoint NOT advanced.
        assert!(thread_run.load_thread("t-3").await.unwrap().is_none());
        // Event store state unchanged from pre-commit snapshot.
        let post_count = events.count(EventScope::run("r-3")).await.unwrap();
        assert_eq!(post_count, pre_count);
    }

    #[tokio::test]
    async fn commit_rolls_back_partial_appends_when_later_draft_fails() {
        let (coord, thread_run, events, _outbox) = build_coordinator();

        // Seed an idempotent event so the second draft collides.
        let seeded_draft = sample_draft("ToolCallReady", "t-4", "r-4");
        let collide_opts = AppendOptions {
            writer_id: Some("runtime".to_string()),
            idempotency_key: Some("k-second".to_string()),
            ..Default::default()
        };
        events
            .append(seeded_draft, collide_opts.clone())
            .await
            .unwrap();

        let mut conflicting_second = sample_draft("ToolCallReady", "t-4", "r-4");
        conflicting_second.payload = json!({"kind": "ToolCallReady", "diff": true});

        let plan =
            CheckpointCommitPlan::checkpoint_only("t-4", Vec::new(), run_record("t-4", "r-4"))
                .with_canonical_drafts(vec![
                    // First draft would succeed in isolation.
                    StagedCanonicalEvent::new(sample_draft("RunStarted", "t-4", "r-4")),
                    // Second draft conflicts via idempotency.
                    StagedCanonicalEvent::new(conflicting_second).with_options(collide_opts),
                ]);

        let pre_count = events.count(EventScope::run("r-4")).await.unwrap();

        let result = coord.commit_checkpoint(plan).await;
        assert!(matches!(result, Err(CommitError::EventAppend(_))));

        // The first draft's append must have been rolled back.
        let post_count = events.count(EventScope::run("r-4")).await.unwrap();
        assert_eq!(
            post_count, pre_count,
            "first append should be rolled back when second fails"
        );
        // Checkpoint never advanced.
        assert!(thread_run.load_thread("t-4").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn staged_events_in_plan_round_trip() {
        let (coord, _thread_run, events, _outbox) = build_coordinator();
        let staged = vec![
            StagedCanonicalEvent::new(sample_draft("RunStarted", "t-5", "r-5")),
            StagedCanonicalEvent::new(sample_draft("RunCompleted", "t-5", "r-5")),
        ];

        let plan =
            CheckpointCommitPlan::checkpoint_only("t-5", Vec::new(), run_record("t-5", "r-5"))
                .with_canonical_drafts(staged);

        coord.commit_checkpoint(plan).await.unwrap();
        let count = events.count(EventScope::run("r-5")).await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn scope_is_stable_for_repeated_calls() {
        let (coord, _, _, _) = build_coordinator();
        assert_eq!(coord.scope(), coord.scope());
    }

    #[tokio::test]
    async fn scope_differs_for_distinct_store_instances() {
        let (coord_a, _, _, _) = build_coordinator();
        let (coord_b, _, _, _) = build_coordinator();
        assert_ne!(coord_a.scope(), coord_b.scope());
    }
}
