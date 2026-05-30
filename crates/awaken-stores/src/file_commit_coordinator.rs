//! File-backed [`CommitCoordinator`] (ADR-0038).
//!
//! Pairs a [`FileStore`] thread/run store with in-memory event and outbox
//! stores so that local file-backed deployments satisfy ADR-0038 D7
//! ("checkpoint writes flow through a coordinator whose `thread_run_store`
//! matches the mailbox `run_store`").
//!
//! Atomicity is best-effort: event and outbox state are snapshotted and
//! restored on failure exactly like [`MemoryCommitCoordinator`], but the
//! on-disk thread/run state is not rolled back when later steps fail.
//! Failed commits may therefore leave the file store advanced past the
//! event/outbox state. This matches FileStore's dev/local use profile —
//! callers needing strict cross-store atomicity must use
//! [`PgCommitCoordinator`](crate::PgCommitCoordinator).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_server_contract::contract::commit_coordinator::{
    Checkpoint, CheckpointCommitOutcome, CommitCoordinator, CommitError, TransactionScopeId,
};
use awaken_server_contract::contract::staged_commit::{
    CheckpointStagedWrites, StagedCommitCoordinator,
};
use awaken_server_contract::contract::storage::{ThreadRunStore, ThreadStore};
use tokio::sync::Mutex;

use crate::commit_batch::run_commit_batch;
use crate::file::FileStore;
use crate::memory_event_store::InMemoryEventStore;
use crate::memory_outbox::InMemoryOutboxStore;

pub const ALLOW_DEV_FILE_COORDINATOR_ENV: &str = "AWAKEN_ALLOW_DEV_FILE_COORDINATOR";

#[derive(Clone)]
pub struct FileCommitCoordinator {
    thread_run: Arc<FileStore>,
    events: Arc<InMemoryEventStore>,
    outbox: Arc<InMemoryOutboxStore>,
    commit_lock: Arc<Mutex<()>>,
    scope: TransactionScopeId,
}

impl FileCommitCoordinator {
    pub fn new(
        thread_run: Arc<FileStore>,
        events: Arc<InMemoryEventStore>,
        outbox: Arc<InMemoryOutboxStore>,
    ) -> Result<Self, CommitError> {
        let allow_env = std::env::var(ALLOW_DEV_FILE_COORDINATOR_ENV).ok();
        if !file_coordinator_allowed(cfg!(debug_assertions), allow_env.as_deref()) {
            return Err(CommitError::Validation(format!(
                "FileCommitCoordinator is dev-only; set {ALLOW_DEV_FILE_COORDINATOR_ENV}=true to opt in explicitly"
            )));
        }
        tracing::warn!(
            env = ALLOW_DEV_FILE_COORDINATOR_ENV,
            "FileCommitCoordinator is dev-only and provides best-effort atomicity; use PgCommitCoordinator for production"
        );
        let scope_descriptor = format!(
            "file::({:p},{:p},{:p})",
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

    /// Wrap an existing [`FileStore`] with fresh in-memory event/outbox
    /// stores. Intended for dev backends and tests that already hold a
    /// shared file-backed thread-run store.
    pub fn wrap(store: Arc<FileStore>) -> Result<Arc<Self>, CommitError> {
        let events = Arc::new(InMemoryEventStore::new());
        let outbox = Arc::new(InMemoryOutboxStore::new());
        Self::new(store, events, outbox).map(Arc::new)
    }
}

fn file_coordinator_allowed(debug_assertions: bool, allow_env: Option<&str>) -> bool {
    debug_assertions
        || allow_env.is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

#[async_trait]
impl CommitCoordinator for FileCommitCoordinator {
    fn scope(&self) -> TransactionScopeId {
        self.scope.clone()
    }

    fn reader(&self) -> Arc<dyn awaken_server_contract::contract::storage::RuntimeCheckpointStore> {
        Arc::new(
            awaken_server_contract::contract::storage::ThreadRunCheckpointStore::new(Arc::clone(
                &self.thread_run,
            )
                as Arc<dyn ThreadRunStore>),
        )
    }

    async fn commit_checkpoint(
        &self,
        plan: Checkpoint,
    ) -> Result<CheckpointCommitOutcome, CommitError> {
        self.commit_checkpoint_staged(plan, CheckpointStagedWrites::default())
            .await
    }
}

#[async_trait]
impl StagedCommitCoordinator for FileCommitCoordinator {
    async fn commit_checkpoint_staged(
        &self,
        plan: Checkpoint,
        staged: CheckpointStagedWrites,
    ) -> Result<CheckpointCommitOutcome, CommitError> {
        plan.validate()?;
        staged.validate(&plan.thread_id, &plan.run.run_id)?;

        let _guard = self.commit_lock.lock().await;

        // FileStore checkpoint writes are not snapshot/restored; see module
        // docs. Event/outbox restore is provided by the shared helper.
        let thread_run = self.thread_run.clone();
        let plan_ref = &plan;
        let write_thread_run = || async move {
            thread_run
                .checkpoint_append(
                    &plan_ref.thread_id,
                    &plan_ref.messages,
                    plan_ref.expected_message_version,
                    &plan_ref.run,
                )
                .await
                .map(|_| ())?;
            if let Some(thread_state) = &plan_ref.thread_state {
                thread_run
                    .save_thread_state(&plan_ref.thread_id, thread_state)
                    .await?;
            }
            Ok(())
        };

        let outcome = run_commit_batch(&staged, &self.events, &self.outbox, write_thread_run).await;
        outcome.map_err(|error| error.reclassify_append_conflict(&plan.thread_id))
    }
}

#[cfg(test)]
mod tests {
    //! Failure-path coverage for [`FileCommitCoordinator`].
    //!
    //! The shared `run_commit_batch` helper has its own isolated unit tests
    //! (see `crate::commit_batch::tests`); the cases below validate that
    //! `FileCommitCoordinator` correctly:
    //!
    //! - hands an idempotency conflict through the helper and returns
    //!   `CommitError::EventAppend`, leaving the underlying `FileStore`
    //!   thread/run state untouched,
    //! - documents (via assertion) that `FileStore` checkpoint writes do
    //!   **not** roll back when later steps fail — this is the trade-off
    //!   that file-backed deployments accept relative to
    //!   `MemoryCommitCoordinator` / `PgCommitCoordinator`, and
    //! - successfully completes a happy-path commit.
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::tempdir;
    use tokio::time::{Duration, sleep};

    /// Monotonic counter so each test gets unique thread/run ids without
    /// pulling in the `v4` / `v7` feature flags on the `uuid` crate.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    fn unique(prefix: &str) -> String {
        format!("{}-{}", prefix, COUNTER.fetch_add(1, Ordering::SeqCst))
    }

    use awaken_server_contract::contract::commit_coordinator::{
        Checkpoint, CommitError, StagedCanonicalEvent,
    };
    use awaken_server_contract::contract::event_store::{
        AppendOptions, CanonicalEventDraft, CanonicalEventKind, EventReader, EventScope,
        EventVisibility, EventWriter,
    };
    use awaken_server_contract::contract::lifecycle::RunStatus;
    use awaken_server_contract::contract::outbox::OutboxMessageDraft;
    use awaken_server_contract::contract::staged_commit::{
        CheckpointStagedWrites, StagedCommitCoordinator,
    };
    use awaken_server_contract::contract::storage::{RunRecord, RunStore, ThreadStore};

    use super::{FileCommitCoordinator, file_coordinator_allowed};
    use crate::file::FileStore;

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

    fn run_record(thread_id: &str, run_id: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.into(),
            thread_id: thread_id.into(),
            agent_id: "agent-test".into(),
            status: RunStatus::Done,
            ..Default::default()
        }
    }

    /// Use a fresh tempdir per test so concurrent runs don't share file
    /// state; the dir is kept alive via the returned `_dir` guard.
    fn coordinator_with_store() -> (
        Arc<FileCommitCoordinator>,
        Arc<FileStore>,
        tempfile::TempDir,
    ) {
        let dir = tempdir().expect("tempdir");
        let store = Arc::new(FileStore::new(dir.path()));
        let coordinator =
            FileCommitCoordinator::wrap(Arc::clone(&store)).expect("file coordinator constructs");
        (coordinator, store, dir)
    }

    #[test]
    fn file_coordinator_guard_allows_debug_build_without_env() {
        assert!(file_coordinator_allowed(true, None));
    }

    #[test]
    fn file_coordinator_guard_rejects_release_without_explicit_env() {
        assert!(!file_coordinator_allowed(false, None));
        assert!(!file_coordinator_allowed(false, Some("")));
        assert!(!file_coordinator_allowed(false, Some("false")));
    }

    #[test]
    fn file_coordinator_guard_allows_release_with_explicit_env() {
        assert!(file_coordinator_allowed(false, Some("true")));
        assert!(file_coordinator_allowed(false, Some("1")));
        assert!(file_coordinator_allowed(false, Some("YES")));
        assert!(file_coordinator_allowed(false, Some(" on ")));
    }

    /// Happy path: a clean plan commits the FileStore thread/run record
    /// and produces ids for staged canonical drafts.
    #[tokio::test]
    async fn happy_path_persists_thread_run_and_canonical_events() {
        let (coord, store, _dir) = coordinator_with_store();
        let thread_id = unique("thread");
        let run_id = unique("run");

        let plan = Checkpoint::checkpoint_only(thread_id.clone(), run_record(&thread_id, &run_id));
        let staged = CheckpointStagedWrites::default().with_canonical_drafts(vec![
            StagedCanonicalEvent::new(sample_draft("RunStarted", &thread_id, &run_id)),
        ]);

        let outcome = coord
            .commit_checkpoint_staged(plan, staged)
            .await
            .expect("file-backed commit succeeds");
        assert_eq!(outcome.canonical_event_ids.len(), 1);
        // FileStore-backed run record is persisted through the coordinator.
        let loaded = store
            .latest_run(&thread_id)
            .await
            .expect("read latest_run")
            .expect("run should exist after commit");
        assert_eq!(loaded.run_id, run_id);
    }

    /// On a canonical-event idempotency conflict the coordinator must
    /// return `EventAppend` and must not advance the FileStore thread/run
    /// state. This proves the event-side rollback is wired even with a
    /// non-rolling file backend.
    #[tokio::test]
    async fn event_append_conflict_returns_error_and_skips_thread_run_write() {
        let (coord, store, _dir) = coordinator_with_store();
        let thread_id = unique("thread");
        let run_id = unique("run");

        // Pre-seed an idempotent event so a second draft with the same
        // identity but a different payload collides.
        let opts = AppendOptions {
            writer_id: Some("writer".into()),
            idempotency_key: Some("k-collide".into()),
            ..Default::default()
        };
        coord
            .events
            .append(
                sample_draft("RunStarted", &thread_id, &run_id),
                opts.clone(),
            )
            .await
            .expect("seed event");

        let mut collide = sample_draft("RunStarted", &thread_id, &run_id);
        collide.payload = serde_json::json!({"kind": "RunStarted", "diff": true});
        let plan = Checkpoint::checkpoint_only(thread_id.clone(), run_record(&thread_id, &run_id));
        let staged = CheckpointStagedWrites::default()
            .with_canonical_drafts(vec![StagedCanonicalEvent::new(collide).with_options(opts)]);

        let result = coord.commit_checkpoint_staged(plan, staged).await;
        assert!(
            matches!(result, Err(CommitError::EventAppend(_))),
            "expected EventAppend variant, got {:?}",
            result
        );

        // FileStore must have no record of `run_id` — the thread-run
        // write is gated behind a successful event-append loop.
        let loaded = store.load_run(&run_id).await.expect("read load_run");
        assert!(
            loaded.is_none(),
            "FileStore thread-run write must not happen when event append fails"
        );
    }

    /// Document the trade-off: FileStore checkpoint writes are NOT
    /// rolled back. If a write to FileStore succeeds and a later step
    /// (e.g. a concurrent commit failing on a different coordinator)
    /// would have triggered restore, the disk state remains advanced.
    ///
    /// We exercise this directly by issuing two commits whose canonical
    /// events conflict on identity. The first succeeds wholesale (thread
    /// row + event). The second collides on idempotency: it fails before
    /// reaching the file write, so the first commit's on-disk state is
    /// preserved — visible state proves "no global rollback across
    /// commits". This is the FileStore design boundary.
    #[tokio::test]
    async fn file_backend_does_not_roll_back_committed_thread_run_state() {
        let (coord, store, _dir) = coordinator_with_store();
        let thread_id = unique("thread");
        let first_run = unique("run");
        let second_run = unique("run");

        // First commit completes successfully.
        let opts = AppendOptions {
            writer_id: Some("writer".into()),
            idempotency_key: Some("k-shared".into()),
            ..Default::default()
        };
        let first_plan =
            Checkpoint::checkpoint_only(thread_id.clone(), run_record(&thread_id, &first_run));
        let first_staged = CheckpointStagedWrites::default().with_canonical_drafts(vec![
            StagedCanonicalEvent::new(sample_draft("RunStarted", &thread_id, &first_run))
                .with_options(opts.clone()),
        ]);
        coord
            .commit_checkpoint_staged(first_plan, first_staged)
            .await
            .expect("first commit succeeds");
        // Disk now has the first run.
        let after_first = store
            .latest_run(&thread_id)
            .await
            .expect("read latest_run")
            .expect("run after first commit");
        assert_eq!(after_first.run_id, first_run);

        // Tiny sleep so file mtimes deterministically order the records.
        sleep(Duration::from_millis(10)).await;

        // Second commit collides on the same `idempotency_key` with a
        // different payload — fails at the event-append step.
        let mut collide = sample_draft("RunStarted", &thread_id, &second_run);
        collide.payload = serde_json::json!({"kind": "RunStarted", "diff": true});
        let second_plan =
            Checkpoint::checkpoint_only(thread_id.clone(), run_record(&thread_id, &second_run));
        let second_staged = CheckpointStagedWrites::default()
            .with_canonical_drafts(vec![StagedCanonicalEvent::new(collide).with_options(opts)]);
        let result = coord
            .commit_checkpoint_staged(second_plan, second_staged)
            .await;
        assert!(matches!(result, Err(CommitError::EventAppend(_))));

        // First commit's on-disk state is preserved (no spurious cleanup
        // from the second commit's rollback path).
        let still_first = store
            .latest_run(&thread_id)
            .await
            .expect("read latest_run")
            .expect("first commit's run must still be on disk");
        assert_eq!(still_first.run_id, first_run);
    }

    /// On `OutboxInsert` failure the in-memory event/outbox state must
    /// roll back; the FileStore thread/run write must not run.
    #[tokio::test]
    async fn outbox_failure_rolls_back_events_and_skips_file_write() {
        let (coord, store, _dir) = coordinator_with_store();
        let thread_id = unique("thread");
        let run_id = unique("run");

        // Construct an outbox draft that passes constructor validation
        // but fails at enqueue time (empty lane after bypass).
        let mut bad = OutboxMessageDraft::new("lane", "target", serde_json::json!({})).unwrap();
        bad.lane.clear();

        let plan = Checkpoint::checkpoint_only(thread_id.clone(), run_record(&thread_id, &run_id));
        let staged = CheckpointStagedWrites::default()
            .with_canonical_drafts(vec![StagedCanonicalEvent::new(sample_draft(
                "RunStarted",
                &thread_id,
                &run_id,
            ))])
            .with_additional_outbox(vec![bad]);

        let result = coord.commit_checkpoint_staged(plan, staged).await;
        assert!(
            matches!(result, Err(CommitError::OutboxInsert(_))),
            "expected OutboxInsert variant, got {:?}",
            result
        );

        // FileStore must be untouched and the canonical event appended
        // before the outbox step must be rolled back.
        assert!(
            store.load_run(&run_id).await.expect("load_run").is_none(),
            "FileStore must not see a thread-run write when outbox fails"
        );
        let event_count = coord
            .events
            .count(EventScope::run(&run_id))
            .await
            .expect("count events");
        assert_eq!(
            event_count, 0,
            "events appended before outbox failure must rollback"
        );
    }

    /// A commit carrying thread-scoped state persists it to the FileStore
    /// in the same coordinator transaction, and a later commit without
    /// thread_state leaves the persisted value untouched.
    #[tokio::test]
    async fn commit_persists_thread_state_and_leaves_it_untouched_when_absent() {
        let (coord, store, _dir) = coordinator_with_store();
        let thread_id = unique("thread");
        let run_id = unique("run");
        let state = awaken_server_contract::PersistedState {
            revision: 9,
            extensions: Default::default(),
        };

        let plan = Checkpoint::checkpoint_only(thread_id.clone(), run_record(&thread_id, &run_id))
            .with_thread_state(state.clone());
        coord
            .commit_checkpoint_staged(plan, CheckpointStagedWrites::default())
            .await
            .expect("commit with thread_state succeeds");
        assert_eq!(
            ThreadStore::load_thread_state(store.as_ref(), &thread_id)
                .await
                .expect("load thread_state"),
            Some(state.clone())
        );

        // A subsequent commit without thread_state must not clear it.
        let run_id2 = unique("run");
        coord
            .commit_checkpoint_staged(
                Checkpoint::checkpoint_only(thread_id.clone(), run_record(&thread_id, &run_id2)),
                CheckpointStagedWrites::default(),
            )
            .await
            .expect("commit without thread_state succeeds");
        assert_eq!(
            ThreadStore::load_thread_state(store.as_ref(), &thread_id)
                .await
                .expect("reload thread_state"),
            Some(state)
        );
    }
}
