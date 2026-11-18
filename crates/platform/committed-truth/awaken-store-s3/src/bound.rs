// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Binds commit and recovery ports to the selected Session partition and claim.
//! Runtime-facing ports bound to one exact claim. Synchronous reads use a materialized view.
use crate::CommittedProjection;
use crate::{Error, Result, SessionStorage};
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
            coordinator::Error as CommitError,
            operation::{CommitOperation, CommitOperationId, CommitReceipt, commit_payload_hash},
            staged::{CommitRecord, ThreadCommit},
        },
        read::{
            checkpoint::{CheckpointReader, EventScope},
            committed_thread_view::CommittedThreadView,
            recovery::{RecoveryError, RunRecoverySnapshot, RunRecoverySource},
            transcript::TranscriptView,
        },
    },
};
use awaken_worker_contract::RunClaim;
use std::{collections::BTreeMap, sync::Arc};

struct Projection {
    generation: u64,
    kernel: CommittedProjection,
    messages: BTreeMap<String, Vec<Message>>,
}
#[derive(Clone)]
pub struct ClaimedSession {
    session: SessionStorage,
    claim: RunClaim,
    projection: Arc<awaken_sync::LeafRwLock<Projection>>,
}
impl SessionStorage {
    pub async fn bind(&self, claim: RunClaim) -> Result<ClaimedSession> {
        let (head, _) = self.inner.load_head(&self.key).await?;
        self.require_claim(&head, &claim)?;
        let projection = self.project(&head).await?;
        Ok(ClaimedSession {
            session: self.clone(),
            claim,
            projection: Arc::new(awaken_sync::LeafRwLock::new(projection)),
        })
    }
    async fn project(&self, head: &crate::model::Head) -> Result<Projection> {
        let kernel = self.replay(head).await?;
        let mut messages = BTreeMap::new();
        for thread in head.threads.keys() {
            let parts = self
                .view_parts(head, thread, TranscriptView::RawCommitted)
                .await?;
            messages.insert(thread.clone(), self.materialize(&parts).await?);
        }
        Ok(Projection {
            generation: head.generation,
            kernel,
            messages,
        })
    }
}
impl ClaimedSession {
    pub fn claim(&self) -> &RunClaim {
        &self.claim
    }
    pub fn session(&self) -> &SessionStorage {
        &self.session
    }
    pub async fn refresh(&self) -> Result<()> {
        let (head, _) = self.session.inner.load_head(&self.session.key).await?;
        self.session.require_claim(&head, &self.claim)?;
        let next = self.session.project(&head).await?;
        let mut current = self.projection.write();
        if next.generation > current.generation {
            *current = next;
        }
        Ok(())
    }
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Projection> {
        self.projection.read()
    }
}
impl ClaimedSession {
    pub async fn commit(
        &self,
        commit: ThreadCommit,
    ) -> std::result::Result<CommitRecord, CommitError> {
        if commit.run_id() != &self.claim.run_id {
            return Err(CommitError::Rejected("bound claim Run mismatch".into()));
        }
        let snapshot = self
            .session
            .recovery_snapshot(&commit.thread_id, commit.run_id())
            .await
            .map_err(|error| CommitError::Rejected(error.to_string()))?;
        let operation = CommitOperation {
            operation_id: CommitOperationId::new(
                self.claim.run_id.clone(),
                snapshot.next_commit_ordinal,
            ),
            expected_thread_version: snapshot.thread_version,
            payload_hash: commit_payload_hash(&commit)
                .map_err(|error| CommitError::Rejected(error.to_string()))?,
            commit,
        };
        self.commit_operation(operation)
            .await
            .map(|receipt| receipt.commit_record())
    }
}
impl ClaimedSession {
    pub async fn commit_operation(
        &self,
        operation: CommitOperation,
    ) -> std::result::Result<CommitReceipt, CommitError> {
        let receipt = self
            .session
            .commit(&self.claim, operation)
            .await
            .map_err(commit_error)?;
        self.refresh().await.map_err(commit_error)?;
        Ok(receipt)
    }
}
impl CommittedThreadView for ClaimedSession {
    fn committed_messages(&self, thread: &ThreadId) -> Vec<Message> {
        self.read()
            .messages
            .get(&thread.0)
            .cloned()
            .unwrap_or_default()
    }
    fn run(&self, run: &RunId) -> Option<RunRecord> {
        self.read().kernel.run(run)
    }
    fn latest_run(&self, thread: &ThreadId) -> Option<RunRecord> {
        self.read().kernel.latest_run(thread)
    }
    fn resume_ticket(&self, run: &RunId) -> Option<ResumeTicket> {
        self.read().kernel.resume_ticket(run)
    }
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        self.read().kernel.open_wait_for_thread(thread)
    }
    fn committed_state(&self, thread: &ThreadId) -> Vec<Command> {
        self.read().kernel.committed_state(thread)
    }
}
#[async_trait::async_trait]
impl RunRecoverySource for ClaimedSession {
    async fn recovery_snapshot_in_session(
        &self,
        session: &ThreadId,
        thread: &ThreadId,
        run: &RunId,
    ) -> std::result::Result<RunRecoverySnapshot, RecoveryError> {
        self.session
            .recovery_snapshot_in_session(session, thread, run)
            .await
    }
    async fn recovery_snapshot(
        &self,
        thread: &ThreadId,
        run: &RunId,
    ) -> std::result::Result<RunRecoverySnapshot, RecoveryError> {
        self.session.recovery_snapshot(thread, run).await
    }
}
#[async_trait::async_trait]
impl CheckpointReader for ClaimedSession {
    async fn list_events(
        &self,
        scope: &EventScope,
        from: Option<u64>,
        limit: usize,
    ) -> std::result::Result<Vec<awaken_agent_contract::audit::record::Record>, String> {
        self.session.list_events(scope, from, limit).await
    }
}
fn commit_error(error: Error) -> CommitError {
    match error {
        Error::Capacity(detail) => CommitError::CapacityExhausted(detail),
        error => CommitError::Rejected(error.to_string()),
    }
}

#[async_trait::async_trait]
impl awaken_dispatch_contract::ClaimedRunCommit<CommitError> for ClaimedSession {
    async fn commit_operation(
        &self,
        command: awaken_dispatch_contract::ClaimedCommitCommand,
    ) -> std::result::Result<CommitReceipt, CommitError> {
        if command.claim != self.claim {
            return Err(CommitError::Rejected("bound claim mismatch".into()));
        }
        ClaimedSession::commit_operation(self, command.operation).await
    }
}

/// Materialized read projection opened before a Worker receives a claim.
/// Authoritative reads refresh from S3; this projection grants no write right.
#[derive(Clone)]
pub struct SessionProjection {
    session: SessionStorage,
    projection: Arc<awaken_sync::LeafRwLock<Projection>>,
}
impl SessionStorage {
    pub async fn open_projection(&self) -> Result<SessionProjection> {
        let (head, _) = self.inner.load_head(&self.key).await?;
        Ok(SessionProjection {
            session: self.clone(),
            projection: Arc::new(awaken_sync::LeafRwLock::new(self.project(&head).await?)),
        })
    }
}
impl SessionProjection {
    pub fn session(&self) -> &SessionStorage {
        &self.session
    }
    pub async fn refresh(&self) -> Result<()> {
        let (head, _) = self.session.inner.load_head(&self.session.key).await?;
        let next = self.session.project(&head).await?;
        let mut current = self.projection.write();
        if next.generation > current.generation {
            *current = next;
        }
        Ok(())
    }
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Projection> {
        self.projection.read()
    }
}

impl CommittedThreadView for SessionProjection {
    fn committed_messages(&self, thread: &ThreadId) -> Vec<Message> {
        self.read()
            .messages
            .get(&thread.0)
            .cloned()
            .unwrap_or_default()
    }
    fn run(&self, run: &RunId) -> Option<RunRecord> {
        self.read().kernel.run(run)
    }
    fn latest_run(&self, thread: &ThreadId) -> Option<RunRecord> {
        self.read().kernel.latest_run(thread)
    }
    fn resume_ticket(&self, run: &RunId) -> Option<ResumeTicket> {
        self.read().kernel.resume_ticket(run)
    }
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        self.read().kernel.open_wait_for_thread(thread)
    }
    fn committed_state(&self, thread: &ThreadId) -> Vec<Command> {
        self.read().kernel.committed_state(thread)
    }
}

#[async_trait::async_trait]
impl RunRecoverySource for SessionProjection {
    async fn recovery_snapshot_in_session(
        &self,
        session: &ThreadId,
        thread: &ThreadId,
        run: &RunId,
    ) -> std::result::Result<RunRecoverySnapshot, RecoveryError> {
        self.session
            .recovery_snapshot_in_session(session, thread, run)
            .await
    }
    async fn recovery_snapshot(
        &self,
        thread: &ThreadId,
        run: &RunId,
    ) -> std::result::Result<RunRecoverySnapshot, RecoveryError> {
        self.session.recovery_snapshot(thread, run).await
    }
}
#[async_trait::async_trait]
impl CheckpointReader for SessionProjection {
    async fn list_events(
        &self,
        scope: &EventScope,
        from: Option<u64>,
        limit: usize,
    ) -> std::result::Result<Vec<awaken_agent_contract::audit::record::Record>, String> {
        self.session.list_events(scope, from, limit).await
    }
}
#[async_trait::async_trait]
impl awaken_dispatch_contract::ClaimedRunCommit<CommitError> for SessionProjection {
    async fn commit_operation(
        &self,
        _command: awaken_dispatch_contract::ClaimedCommitCommand,
    ) -> std::result::Result<CommitReceipt, CommitError> {
        Err(CommitError::Rejected(
            "read projection cannot issue writes; use the selected claimed writer".into(),
        ))
    }
}
