mod support;
use awaken_agent_contract::{
    agent::{
        awaiting::ResumeTicket,
        message::Message,
        run::{Id as RunId, Record as RunRecord},
        state::Command,
        thread::Id as ThreadId,
    },
    thread::{
        commit::{
            coordinator::{Coordinator, Error, OperationCoordinator},
            operation::{CommitOperation, CommitOperationId, CommitReceipt, commit_payload_hash},
            staged::{CommitRecord, ThreadCommit},
        },
        read::{
            checkpoint::{CheckpointReader, EventScope},
            committed_thread_view::CommittedThreadView,
            recovery::{RecoveryError, RunRecoverySnapshot, RunRecoverySource},
        },
    },
};
use awaken_store_s3::CommittedProjection;
use awaken_store_s3::SessionStorage;
use awaken_sync::LeafRwLock;

// Existing backend-neutral laws run over the real object publication boundary.
// Only fixture admission is added here because the older suite has no dispatch port.
struct Harness {
    session: SessionStorage,
    projection: LeafRwLock<CommittedProjection>,
    legacy: tokio::sync::Mutex<()>,
    projection_update: tokio::sync::Mutex<()>,
}
impl Harness {
    async fn new() -> Self {
        let (_, clock, _, session) = support::setup().await;
        clock.advance(1_768_000_000_000);
        Self {
            session,
            projection: LeafRwLock::new(CommittedProjection::default()),
            legacy: tokio::sync::Mutex::new(()),
            projection_update: tokio::sync::Mutex::new(()),
        }
    }
    async fn apply(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        let run = operation.commit.run_id().0.clone();
        let thread = operation.commit.thread_id.0.clone();
        self.session
            .accept(support::dispatch(&run, &thread, "session"))
            .await
            .map_err(reject)?;
        let claim = self
            .session
            .claim(&run, "conformance", 1_000_000)
            .await
            .map_err(reject)?
            .run_claim();
        let result = self.session.commit(&claim, operation).await.map_err(reject);
        {
            let _guard = self.projection_update.lock().await;
            let next = self.session.execution_view().await.map_err(reject)?;
            let mut current = self.projection.write();
            if next.commit_count() > current.commit_count() {
                *current = next;
            }
        }
        let _ = self.session.relinquish(&claim).await;
        result
    }
}
fn reject(error: impl std::fmt::Display) -> Error {
    Error::Rejected(error.to_string())
}
#[async_trait::async_trait]
impl Coordinator for Harness {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        let _guard = self.legacy.lock().await;
        self.session
            .accept(support::dispatch(
                &commit.run_id().0,
                &commit.thread_id.0,
                "session",
            ))
            .await
            .map_err(reject)?;
        let snapshot = self
            .session
            .recovery_snapshot(&commit.thread_id, commit.run_id())
            .await
            .map_err(reject)?;
        self.apply(CommitOperation {
            operation_id: CommitOperationId::new(
                commit.run_id().clone(),
                snapshot.next_commit_ordinal,
            ),
            expected_thread_version: snapshot.thread_version,
            payload_hash: commit_payload_hash(&commit).map_err(reject)?,
            commit,
        })
        .await
        .map(|r| r.commit_record())
    }
}
#[async_trait::async_trait]
impl OperationCoordinator for Harness {
    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        self.apply(operation).await
    }
}
impl CommittedThreadView for Harness {
    fn committed_messages(&self, t: &ThreadId) -> Vec<Message> {
        self.projection.read().committed_messages(t)
    }
    fn run(&self, r: &RunId) -> Option<RunRecord> {
        self.projection.read().run(r)
    }
    fn latest_run(&self, t: &ThreadId) -> Option<RunRecord> {
        self.projection.read().latest_run(t)
    }
    fn resume_ticket(&self, r: &RunId) -> Option<ResumeTicket> {
        self.projection.read().resume_ticket(r)
    }
    fn open_wait_for_thread(&self, t: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        self.projection.read().open_wait_for_thread(t)
    }
    fn committed_state(&self, t: &ThreadId) -> Vec<Command> {
        self.projection.read().committed_state(t)
    }
}
#[async_trait::async_trait]
impl CheckpointReader for Harness {
    async fn list_events(
        &self,
        s: &EventScope,
        f: Option<u64>,
        l: usize,
    ) -> Result<Vec<awaken_agent_contract::audit::record::Record>, String> {
        self.session.list_events(s, f, l).await
    }
}
#[async_trait::async_trait]
impl RunRecoverySource for Harness {
    async fn recovery_snapshot(
        &self,
        t: &ThreadId,
        r: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.session.recovery_snapshot(t, r).await
    }
}
macro_rules! law {
    ($name:ident) => {
        #[tokio::test]
        async fn $name() {
            awaken_store_conformance::$name(&Harness::new().await).await;
        }
    };
}
law!(commit_then_read);
law!(events_ordered_and_paged);
law!(terminal_run_is_fenced);
law!(commits_accumulate);
law!(transcript_snapshots_freeze_append_only_prefix);
law!(resume_ticket_awaits_then_clears);
law!(open_wait_selects_only_the_latest_run);
law!(concurrent_appends_are_dense_and_distinct);
law!(two_threads_in_one_store_are_isolated);
law!(recovery_snapshot_is_consistent);
law!(commit_operation_is_idempotent_and_cas);
#[tokio::test]
async fn operation_receipt_replay_is_thread_bound() {
    let store = Harness::new().await;
    let operation = support::operation("receipt-run", "thread", 0, 0, "original");
    let receipt = store.commit_operation(operation.clone()).await.unwrap();
    awaken_store_conformance::operation_receipt_replay_is_thread_bound(
        &store, &operation, &receipt,
    )
    .await;
}
law!(concurrent_operations_cas_one_winner);
law!(committed_state_replays);
law!(delegation_and_tool_state_commit_atomically);
