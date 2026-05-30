//! Shared checkpoint-commit body for [`crate::MemoryCommitCoordinator`] and
//! [`crate::FileCommitCoordinator`].
//!
//! Drives the event/outbox append loop, with snapshot/restore rollback on
//! failure, and finally hands the thread-run checkpoint write to a caller-
//! supplied closure. The closure is responsible for any backend-specific
//! pre-write snapshotting (the in-memory backend can roll back; FileStore
//! cannot, by design).

use std::future::Future;

use awaken_server_contract::contract::commit_coordinator::{CheckpointCommitOutcome, CommitError};
use awaken_server_contract::contract::event_store::EventWriter;
use awaken_server_contract::contract::outbox::OutboxStore;
use awaken_server_contract::contract::staged_commit::CheckpointStagedWrites;
use awaken_server_contract::contract::storage::StorageError;

use crate::memory_event_store::InMemoryEventStore;
use crate::memory_outbox::InMemoryOutboxStore;

/// Run the standard checkpoint commit batch:
///
/// 1. snapshot event/outbox state for rollback,
/// 2. append canonical drafts in order,
/// 3. append server events in order,
/// 4. enqueue additional outbox drafts,
/// 5. delegate the thread-run checkpoint write to `write_thread_run`,
/// 6. on any failure restore the event/outbox snapshots.
///
/// `write_thread_run` may also perform backend-specific snapshot/restore for
/// the thread-run store; this helper does not touch that state.
pub(crate) async fn run_commit_batch<W, Fut>(
    staged: &CheckpointStagedWrites,
    events: &InMemoryEventStore,
    outbox: &InMemoryOutboxStore,
    write_thread_run: W,
) -> Result<CheckpointCommitOutcome, CommitError>
where
    W: FnOnce() -> Fut,
    Fut: Future<Output = Result<(), StorageError>>,
{
    let event_snapshot = events.snapshot_state().await;
    let outbox_snapshot = outbox.snapshot_state().await;

    let restore = || async {
        events.restore_state(event_snapshot.clone()).await;
        outbox.restore_state(outbox_snapshot.clone()).await;
    };

    let mut canonical_event_ids = Vec::with_capacity(staged.canonical_drafts.len());
    for staged_event in &staged.canonical_drafts {
        match events
            .append(
                staged_event.draft.clone(),
                staged_event.append_options.clone(),
            )
            .await
        {
            Ok(result) => canonical_event_ids.push(result.event.event_id.as_str().to_string()),
            Err(error) => {
                restore().await;
                return Err(CommitError::EventAppend(error));
            }
        }
    }

    let mut server_event_ids = Vec::with_capacity(staged.server_events.len());
    for event in &staged.server_events {
        match events
            .append(event.draft.clone(), event.options.clone())
            .await
        {
            Ok(result) => server_event_ids.push(result.event.event_id.as_str().to_string()),
            Err(error) => {
                restore().await;
                return Err(CommitError::EventAppend(error));
            }
        }
    }

    let mut additional_outbox_ids = Vec::with_capacity(staged.additional_outbox.len());
    for draft in &staged.additional_outbox {
        match outbox.enqueue_outbox(draft.clone()).await {
            Ok(result) => additional_outbox_ids.push(result.message.outbox_id),
            Err(error) => {
                restore().await;
                return Err(CommitError::OutboxInsert(error));
            }
        }
    }

    if let Err(error) = write_thread_run().await {
        restore().await;
        return Err(CommitError::StoreWrite(error));
    }

    Ok(CheckpointCommitOutcome {
        canonical_event_ids,
        server_event_ids,
        additional_outbox_ids,
    })
}

#[cfg(test)]
mod tests {
    //! Isolated rollback verification for the helper independent of either
    //! [`crate::MemoryCommitCoordinator`] or [`crate::FileCommitCoordinator`].
    //! Each test exercises one of the three failure positions:
    //!
    //! 1. `EventAppend` failure during canonical-draft loop,
    //! 2. `OutboxInsert` failure during additional-outbox loop,
    //! 3. `StoreWrite` failure during the caller-supplied `write_thread_run`.
    //!
    //! All three cases must end with empty event/outbox state (rollback) and
    //! a `CommitError` of the correct variant.
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use awaken_server_contract::contract::commit_coordinator::{CommitError, StagedCanonicalEvent};
    use awaken_server_contract::contract::event_store::{
        AppendOptions, CanonicalEventDraft, CanonicalEventKind, EventReader, EventScope,
        EventVisibility, EventWriter,
    };
    use awaken_server_contract::contract::outbox::{OutboxMessageDraft, OutboxStatus, OutboxStore};
    use awaken_server_contract::contract::staged_commit::CheckpointStagedWrites;
    use awaken_server_contract::contract::storage::StorageError;

    use super::run_commit_batch;
    use crate::memory_event_store::InMemoryEventStore;
    use crate::memory_outbox::InMemoryOutboxStore;

    fn sample_draft(kind: &str, thread_id: &str, run_id: &str) -> CanonicalEventDraft {
        let mut draft = CanonicalEventDraft::new(
            vec![EventScope::thread(thread_id), EventScope::run(run_id)],
            CanonicalEventKind::new(kind).unwrap(),
            serde_json::json!({"kind": kind}),
            "test",
        )
        .unwrap();
        draft.visibility = EventVisibility::Public;
        draft
    }

    async fn fresh() -> (Arc<InMemoryEventStore>, Arc<InMemoryOutboxStore>) {
        (
            Arc::new(InMemoryEventStore::new()),
            Arc::new(InMemoryOutboxStore::new()),
        )
    }

    /// On `EventAppend` failure, neither outbox state nor the
    /// `write_thread_run` closure should be touched. The error variant
    /// must be `CommitError::EventAppend`.
    #[tokio::test]
    async fn event_append_failure_rolls_back_and_skips_write() {
        let (events, outbox) = fresh().await;

        // Seed an idempotent event so a colliding append fails.
        let opts = AppendOptions {
            writer_id: Some("writer".into()),
            idempotency_key: Some("k1".into()),
            ..Default::default()
        };
        events
            .append(sample_draft("RunStarted", "t-1", "r-1"), opts.clone())
            .await
            .unwrap();
        let event_count_before = events.count(EventScope::run("r-1")).await.unwrap();

        // Build a plan whose canonical_drafts collide with the seeded draft.
        let mut colliding = sample_draft("RunStarted", "t-1", "r-1");
        colliding.payload = serde_json::json!({"kind": "RunStarted", "diff": true});
        let staged = CheckpointStagedWrites::default().with_canonical_drafts(vec![
            StagedCanonicalEvent::new(colliding).with_options(opts),
        ]);

        let write_called = Arc::new(AtomicBool::new(false));
        let write_called_clone = Arc::clone(&write_called);
        let result = run_commit_batch(&staged, &events, &outbox, || async move {
            write_called_clone.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;

        assert!(
            matches!(result, Err(CommitError::EventAppend(_))),
            "expected EventAppend variant, got {:?}",
            result
        );
        assert!(
            !write_called.load(Ordering::SeqCst),
            "write_thread_run must NOT run after an event-append failure"
        );
        let event_count_after = events.count(EventScope::run("r-1")).await.unwrap();
        assert_eq!(
            event_count_after, event_count_before,
            "event store state must be restored to pre-batch snapshot"
        );
    }

    /// On `OutboxInsert` failure mid-batch, every already-appended event
    /// must be rolled back (event store snapshot restored), and the
    /// `write_thread_run` closure must not run.
    #[tokio::test]
    async fn outbox_failure_rolls_back_events_and_skips_write() {
        let (events, outbox) = fresh().await;

        // Construct an outbox draft that will pass `OutboxMessageDraft::new`
        // validation but fail at enqueue time: we mutate its `lane` to empty
        // bypassing the constructor. The store-side enqueue revalidates.
        let mut bad = OutboxMessageDraft::new("lane", "target", serde_json::json!({})).unwrap();
        bad.lane.clear();

        let staged = CheckpointStagedWrites::default()
            .with_canonical_drafts(vec![StagedCanonicalEvent::new(sample_draft(
                "RunStarted",
                "t-2",
                "r-2",
            ))])
            .with_additional_outbox(vec![bad]);

        let write_called = Arc::new(AtomicBool::new(false));
        let write_called_clone = Arc::clone(&write_called);
        let result = run_commit_batch(&staged, &events, &outbox, || async move {
            write_called_clone.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;

        assert!(
            matches!(result, Err(CommitError::OutboxInsert(_))),
            "expected OutboxInsert variant, got {:?}",
            result
        );
        assert!(
            !write_called.load(Ordering::SeqCst),
            "write_thread_run must NOT run after an outbox failure"
        );
        // The single canonical event appended before the outbox step must
        // be rolled back.
        let event_count = events.count(EventScope::run("r-2")).await.unwrap();
        assert_eq!(
            event_count, 0,
            "events appended before outbox failure must rollback"
        );
    }

    /// On `write_thread_run` failure, every previously appended event and
    /// every enqueued outbox row must be rolled back. The error variant
    /// must be `CommitError::StoreWrite`.
    #[tokio::test]
    async fn write_thread_run_failure_rolls_back_events_and_outbox() {
        let (events, outbox) = fresh().await;

        let staged = CheckpointStagedWrites::default()
            .with_canonical_drafts(vec![StagedCanonicalEvent::new(sample_draft(
                "RunStarted",
                "t-3",
                "r-3",
            ))])
            .with_additional_outbox(vec![
                OutboxMessageDraft::new("lane", "target", serde_json::json!({"k": 1})).unwrap(),
            ]);

        let result = run_commit_batch(&staged, &events, &outbox, || async {
            Err(StorageError::Validation(
                "simulated thread-run write failure".into(),
            ))
        })
        .await;

        assert!(
            matches!(result, Err(CommitError::StoreWrite(_))),
            "expected StoreWrite variant, got {:?}",
            result
        );
        let event_count = events.count(EventScope::run("r-3")).await.unwrap();
        assert_eq!(event_count, 0, "events must rollback on thread-run failure");
        let outbox_remaining = outbox
            .list_outbox(Some(OutboxStatus::Pending), 10)
            .await
            .unwrap();
        assert_eq!(
            outbox_remaining.len(),
            0,
            "outbox drafts must rollback on thread-run failure"
        );
    }

    /// Happy path: when every step succeeds the helper returns ids for
    /// each appended event and enqueued outbox draft.
    #[tokio::test]
    async fn happy_path_returns_ids_and_runs_write() {
        let (events, outbox) = fresh().await;
        let staged = CheckpointStagedWrites::default()
            .with_canonical_drafts(vec![StagedCanonicalEvent::new(sample_draft(
                "RunStarted",
                "t-ok",
                "r-ok",
            ))])
            .with_additional_outbox(vec![
                OutboxMessageDraft::new("lane", "target", serde_json::json!({})).unwrap(),
            ]);

        let write_called = Arc::new(AtomicBool::new(false));
        let write_called_clone = Arc::clone(&write_called);
        let outcome = run_commit_batch(&staged, &events, &outbox, || async move {
            write_called_clone.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect("commit batch must succeed");

        assert_eq!(outcome.canonical_event_ids.len(), 1);
        assert_eq!(outcome.additional_outbox_ids.len(), 1);
        assert!(outcome.server_event_ids.is_empty());
        assert!(write_called.load(Ordering::SeqCst));
    }
}
