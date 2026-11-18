// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Exposes partition-scoped recovery, transcript and commit operations to the runtime.
use crate::{Error, SessionStorage};
use awaken_agent_contract::{
    agent::{run::Id as RunId, thread::Id as ThreadId},
    thread::{
        commit::{coordinator::Error as CommitError, operation::CommitReceipt},
        read::{
            checkpoint::{CheckpointReader, EventScope},
            recovery::{RecoveryError, RunRecoverySnapshot, RunRecoverySource},
        },
    },
};
use awaken_dispatch_contract::{ClaimedCommitCommand, ClaimedRunCommit};
use awaken_runtime_contract::runs::RunError;

#[async_trait::async_trait]
impl ClaimedRunCommit<RunError> for SessionStorage {
    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, RunError> {
        let (head, _) = self.inner.load_head(&self.key).await.map_err(run_error)?;
        self.require_claim(&head, &command.claim)
            .map_err(run_error)?;
        awaken_agent_contract::thread::commit::operation::require_commit_operation_payload_hash(
            &command.operation,
        )
        .map_err(|error| run_error(Error::Identity(error.to_string())))?;
        self.commit(&command.claim, command.operation)
            .await
            .map_err(run_error)
    }
}
#[async_trait::async_trait]
impl ClaimedRunCommit<CommitError> for SessionStorage {
    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, CommitError> {
        self.commit(&command.claim, command.operation)
            .await
            .map_err(|error| match error {
                Error::Capacity(detail) => CommitError::CapacityExhausted(detail),
                error => CommitError::Rejected(error.to_string()),
            })
    }
}
#[async_trait::async_trait]
impl RunRecoverySource for SessionStorage {
    async fn recovery_snapshot(
        &self,
        thread: &ThreadId,
        run: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        let (head, _) = self
            .inner
            .load_head(&self.key)
            .await
            .map_err(recovery_error)?;
        let kernel = self.replay(&head).await.map_err(recovery_error)?;
        let mut snapshot = kernel.recovery_snapshot(thread, run).await?;
        if let Some(base) = head.threads.get(&thread.0).and_then(|t| t.base.as_ref()) {
            let frozen: crate::model::FrozenView = self
                .inner
                .read_json(&self.key, base)
                .await
                .map_err(recovery_error)?;
            let mut messages = self
                .materialize(&frozen.parts)
                .await
                .map_err(recovery_error)?;
            let mut cursors = vec![0; messages.len()];
            messages.extend(snapshot.messages);
            cursors.extend(snapshot.message_commit_cursors);
            snapshot.messages = messages;
            snapshot.message_commit_cursors = cursors;
        }
        Ok(snapshot)
    }
    async fn recovery_snapshot_in_session(
        &self,
        session: &ThreadId,
        thread: &ThreadId,
        run: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        if session.0 != self.key.session {
            return Err(recovery_error(Error::Invalid(
                "recovery Session mismatch".into(),
            )));
        }
        self.recovery_snapshot(thread, run).await
    }
}
#[async_trait::async_trait]
impl CheckpointReader for SessionStorage {
    async fn list_events(
        &self,
        scope: &EventScope,
        from: Option<u64>,
        limit: usize,
    ) -> Result<Vec<awaken_agent_contract::audit::record::Record>, String> {
        let (head, _) = self
            .inner
            .load_head(&self.key)
            .await
            .map_err(|e| e.to_string())?;
        self.replay(&head)
            .await
            .map_err(|e| e.to_string())?
            .list_events(scope, from, limit)
            .await
    }
}
pub(crate) fn run_error(error: Error) -> RunError {
    use awaken_agent_contract::thread::commit::operation::CommitOperationAdmissionError;
    use awaken_dispatch_contract::{
        COMMIT_CONFLICT, COMMIT_FENCED, COMMIT_IDENTITY_MISMATCH, COMMIT_OUTCOME_UNKNOWN,
    };
    match error {
        Error::Fenced => RunError::bad_request_classified(COMMIT_FENCED, "claim is fenced"),
        Error::Admission(
            error @ (CommitOperationAdmissionError::ThreadVersionConflict { .. }
            | CommitOperationAdmissionError::OrdinalConflict { .. }),
        ) => RunError::conflict_classified(COMMIT_CONFLICT, error.to_string()),
        Error::Admission(error) => {
            RunError::bad_request_classified(COMMIT_IDENTITY_MISMATCH, error.to_string())
        }
        Error::Identity(detail) => {
            RunError::bad_request_classified(COMMIT_IDENTITY_MISMATCH, detail)
        }
        Error::Conflict(detail) => RunError::conflict_classified(COMMIT_CONFLICT, detail),
        Error::OutcomeUnknown(detail) => {
            RunError::unavailable_classified(COMMIT_OUTCOME_UNKNOWN, detail)
        }
        Error::Invalid(detail) => RunError::bad_request(detail),
        Error::Corrupt(detail) => RunError::internal(detail),
        other => RunError::unavailable(other.to_string()),
    }
}
fn recovery_error(error: Error) -> RecoveryError {
    RecoveryError::Rejected(error.to_string())
}
