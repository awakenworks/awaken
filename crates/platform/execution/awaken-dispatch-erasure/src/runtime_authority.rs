// @formal surface obligation=remote_worker.committed_truth_is_coordinator_owned
// @formal reason: The selected runtime authority and adapter retain the existing commit fence.
// @formal reason: Concrete pools/stores/Clock/initialization remain Coordinator-owned; Host local/remote
// @formal reason: permission and commit decisions stay in SessionAuthority. This module opens no backend.
//! The existing durable capability selected by process composition.

use crate::{AnyCoordinatorDispatch, LocalCommit};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator;
use awaken_agent_contract::thread::commit::operation::{
    CommitReceipt, require_commit_operation_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::lifecycle::RunLifecycleFeed;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_agent_contract::thread::read::transcript::{TranscriptPage, TranscriptPageRequest};
use awaken_dispatch_contract::{
    ClaimedCommitCommand, ClaimedRunCommit, DispatchCommitAuthority, RunDispatch,
};
use awaken_runtime_contract::runs::RunError;
use std::future::Future;
use std::sync::Arc;

/// The complete durable capability injected into one Coordinator Runtime Host.
///
/// Source-owner rule A3: composition consumes this original canonical declaration;
/// it does not select a backend or manufacture an implementation here. Existing
/// Coordinator/Host tables retain the runtime authorization and failure laws.
/// ```
/// use std::sync::Arc;
/// use awaken_dispatch_erasure::{RuntimeAuthority, RuntimeAuthorityError};
/// fn selected(authority: Arc<dyn RuntimeAuthority>) -> Arc<dyn RuntimeAuthority> { authority }
/// let _: RuntimeAuthorityError = RuntimeAuthorityError::unavailable("source input");
/// ```
#[async_trait::async_trait]
pub trait RuntimeAuthority: Send + Sync {
    async fn open_commit(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn LocalCommit>, RuntimeAuthorityError>;

    async fn durable_thread_exists(&self, thread: &str) -> Result<bool, RuntimeAuthorityError>;

    /// Page an authorized logical Thread in its parent Session partition.
    async fn read_transcript_page(
        &self,
        session: &str,
        request: &TranscriptPageRequest,
    ) -> Result<TranscriptPage, RunError> {
        self.open_commit(session)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?
            .authoritative_transcript_page(request)
            .await
    }

    /// Select the versioned writer for authenticated Worker commits.
    fn run_commits(self: Arc<Self>) -> Arc<dyn ClaimedRunCommit<RunError>>
    where
        Self: 'static,
    {
        guarded_runtime_commits(self)
    }

    /// Process-wide recovery, when the backend has one shared commit space.
    fn shared_recovery_source(&self) -> Option<Arc<dyn RunRecoverySource>> {
        None
    }

    /// Process-wide lifecycle order, when the backend has one shared commit space.
    fn shared_lifecycle_feed(&self) -> Option<Arc<dyn RunLifecycleFeed>> {
        None
    }

    fn dispatch_store(&self) -> Arc<AnyCoordinatorDispatch>;

    fn dispatch_wake(&self) -> Option<Arc<dyn awaken_dispatch_contract::WakeSignal>>;

    fn stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn StreamCheckpointStore>, RuntimeAuthorityError>;
}

pub fn guarded_runtime_commits<T: RuntimeAuthority + ?Sized + 'static>(
    runtime: Arc<T>,
) -> Arc<dyn ClaimedRunCommit<RunError>> {
    Arc::new(RuntimeRunCommits(runtime))
}

struct RuntimeRunCommits<T: ?Sized>(Arc<T>);

#[async_trait::async_trait]
impl<T: RuntimeAuthority + ?Sized> ClaimedRunCommit<RunError> for RuntimeRunCommits<T> {
    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, RunError> {
        commit_claimed_operation(
            self.0.dispatch_store().as_ref(),
            command,
            |session| async move {
                self.0
                    .open_commit(&session.0)
                    .await
                    .map(|owner| owner as Arc<dyn OperationCoordinator>)
                    .map_err(|error| match error {
                        RuntimeAuthorityError::Unavailable(message) => {
                            RunError::unavailable(message)
                        }
                        RuntimeAuthorityError::Corrupt(message)
                        | RuntimeAuthorityError::Misconfigured(message) => {
                            RunError::internal(message)
                        }
                    })
            },
        )
        .await
    }
}

/// Verify that committed logical Run/Thread truth belongs to the dispatch whose
/// exact claim is currently fenced. Both embedded and remote workers cross this
/// helper so topology cannot change the authority check.
pub fn validate_commit_dispatch_binding(
    dispatch: &RunDispatch,
    commit: &ThreadCommit,
) -> Result<(), String> {
    if commit.run_id() != dispatch.run_id() {
        return Err(format!(
            "commit run {} does not match claimed dispatch run {}",
            commit.run_id().0,
            dispatch.run_id().0
        ));
    }
    if &commit.thread_id != dispatch.thread_id() {
        return Err(format!(
            "commit thread {} does not match claimed dispatch thread {}",
            commit.thread_id.0,
            dispatch.thread_id().0
        ));
    }
    Ok(())
}

/// Validate the operation against the trusted Dispatch while holding its fence.
pub fn validate_claimed_operation(
    dispatch: &RunDispatch,
    command: &ClaimedCommitCommand,
) -> Result<(), RunError> {
    if dispatch.run_id() != &command.claim.run_id {
        return Err(RunError::internal("claimed Dispatch row names another Run"));
    }
    let operation = &command.operation;
    validate_commit_dispatch_binding(dispatch, &operation.commit).map_err(RunError::bad_request)?;
    if operation.operation_id.run_id != *dispatch.run_id() {
        return Err(RunError::bad_request(format!(
            "commit operation run {} does not match claimed dispatch run {}",
            operation.operation_id.run_id.0,
            dispatch.run_id().0,
        )));
    }
    require_commit_operation_payload_hash(operation).map_err(|error| {
        RunError::bad_request_classified(
            awaken_dispatch_contract::COMMIT_IDENTITY_MISMATCH,
            error.to_string(),
        )
    })
}

/// Compatibility path for backends that hold an epoch guard across a separate
/// commit call. The guarded Dispatch selects the Session before opening it.
pub async fn commit_claimed_operation<F>(
    dispatch: &dyn DispatchCommitAuthority,
    command: ClaimedCommitCommand,
    resolve: impl FnOnce(ThreadId) -> F + Send,
) -> Result<CommitReceipt, RunError>
where
    F: Future<Output = Result<Arc<dyn OperationCoordinator>, RunError>> + Send,
{
    let guard = dispatch
        .lock_commit_epoch(&command.claim)
        .await
        .map_err(|error| RunError::internal(error.to_string()))?
        .ok_or_else(|| {
            RunError::bad_request_classified(
                awaken_dispatch_contract::COMMIT_FENCED,
                "run claim is stale",
            )
        })?;
    let session = validate_claimed_operation(guard.request(), &command)
        .map(|()| guard.request().session_thread_id().clone());
    let committed = async move {
        let coordinator = resolve(session?).await?;
        coordinator
            .commit_operation(command.operation)
            .await
            .map_err(|error| RunError::internal(error.to_string()))
    }
    .await;
    guard.release_after(committed).await.map_err(|error| {
        RunError::unavailable(format!("commit claim lock release failed: {error}"))
    })?
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeAuthorityError {
    #[error("runtime authority unavailable: {0}")]
    Unavailable(String),
    #[error("runtime authority is corrupt: {0}")]
    Corrupt(String),
    #[error("runtime authority is misconfigured: {0}")]
    Misconfigured(String),
}

impl RuntimeAuthorityError {
    pub fn unavailable(error: impl Into<String>) -> Self {
        Self::Unavailable(error.into())
    }

    pub fn misconfigured(error: impl Into<String>) -> Self {
        Self::Misconfigured(error.into())
    }
}
