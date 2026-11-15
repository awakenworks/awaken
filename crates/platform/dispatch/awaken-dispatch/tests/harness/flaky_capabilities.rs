//! Explicit role fixtures for local/remote scheduler composition.
use super::*;
use awaken_dispatch_contract::{
    ClaimedCheckpointTransport, ClaimedRunCommit, ContinuationAdmission, DispatchCursor,
    DispatchError, DispatchOperationalFeed, DispatchPage, PendingInput, RunDispatch,
    SessionContinuationOutbox, SettleOutcome, WorkerCredentialGrantTransport,
};
use awaken_worker_contract::RunClaim;

#[async_trait::async_trait]
impl SessionContinuationOutbox for FlakyDispatchStore {
    async fn stage_session_resume(
        &self,
        input: PendingInput,
        thread: &ThreadId,
        prior: Option<u64>,
        next: Option<u64>,
    ) -> Result<bool, DispatchError> {
        self.inner
            .stage_session_resume(input, thread, prior, next)
            .await
    }
    async fn relay_and_enqueue(
        &self,
        input: PendingInput,
        request: RunDispatch,
        admission: ContinuationAdmission,
    ) -> Result<(), DispatchError> {
        self.inner
            .relay_and_enqueue(input, request, admission)
            .await
    }
}
#[async_trait::async_trait]
impl DispatchOperationalFeed for FlakyDispatchStore {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<DispatchPage, DispatchError> {
        self.inner.events_after(cursor, limit, run).await
    }
}

// The scheduler matrix has no runnable work. These explicit remote ports must
// never be called: selecting topology itself has zero execution effects.
#[async_trait::async_trait]
impl ClaimedCheckpointTransport for FlakyDispatchStore {
    async fn load_stream_checkpoint(
        &self,
        _: &RunClaim,
    ) -> Result<Option<awaken_agent_contract::stream::checkpoint::StreamCheckpoint>, DispatchError>
    {
        unreachable!("empty scheduler fixture must not read remote checkpoints")
    }
    async fn put_stream_checkpoint(
        &self,
        _: &RunClaim,
        _: awaken_agent_contract::stream::checkpoint::StreamCheckpoint,
    ) -> Result<SettleOutcome, DispatchError> {
        unreachable!("empty scheduler fixture must not write remote checkpoints")
    }
    async fn delete_stream_checkpoint(&self, _: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        unreachable!("empty scheduler fixture must not delete remote checkpoints")
    }
    async fn load_recovery(
        &self,
        _: &RunClaim,
        _: Option<&awaken_session_application_contract::SessionWorkAuthorityReceipt>,
        _: Option<awaken_agent_contract::thread::read::recovery::ThreadPrefixRef>,
    ) -> Result<awaken_agent_contract::thread::read::recovery::LoadedRunRecovery, DispatchError>
    {
        unreachable!("empty scheduler fixture must not read remote recovery")
    }
}
#[async_trait::async_trait]
impl WorkerCredentialGrantTransport for FlakyDispatchStore {
    async fn credential_grant(
        &self,
        _: awaken_dispatch_contract::WorkerCredentialGrant,
    ) -> Result<Option<awaken_dispatch_contract::WorkerCredentialGrantMaterial>, DispatchError>
    {
        unreachable!("empty scheduler fixture must not request credential grants")
    }
}
#[async_trait::async_trait]
impl ClaimedRunCommit for FlakyDispatchStore {
    async fn commit_operation(
        &self,
        _: awaken_dispatch_contract::ClaimedCommitCommand,
    ) -> Result<
        awaken_agent_contract::thread::commit::operation::CommitReceipt,
        awaken_agent_contract::thread::commit::coordinator::Error,
    > {
        unreachable!("empty scheduler fixture must not commit")
    }
}
