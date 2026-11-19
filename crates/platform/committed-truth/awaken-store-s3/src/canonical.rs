// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Publishes canonical versioned operations and their exact receipts through the Session head.
//! Claim-scoped ports over the canonical Dispatch component, without a mirror lease.
use crate::{Error, Result, S3Storage, SessionKey, SessionStorage};
use awaken_agent_contract::observation::{Observed, UnknownCause};
use awaken_agent_contract::stream::checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};
use awaken_agent_contract::thread::commit::{
    coordinator::Error as CommitError, operation::CommitReceipt,
};
use awaken_dispatch_contract::{
    ClaimAuthority, ClaimedCommitCommand, ClaimedRunCommit, ConditionalClaimAuthority,
    DispatchCommitAuthority,
};
use awaken_object_storage::SessionObjectStore as _;
use awaken_runtime_contract::runs::RunError;
use awaken_worker_contract::RunClaim;
use std::sync::Arc;

#[derive(Clone)]
pub struct CanonicalRunCommits {
    storage: S3Storage,
    pub(crate) dispatch: Arc<dyn DispatchCommitAuthority>,
}
impl S3Storage {
    pub fn canonical_commits(
        &self,
        dispatch: Arc<dyn DispatchCommitAuthority>,
    ) -> CanonicalRunCommits {
        CanonicalRunCommits {
            storage: self.clone(),
            dispatch,
        }
    }
    pub fn canonical_checkpoints(
        &self,
        claim: RunClaim,
        dispatch: Arc<dyn DispatchCommitAuthority>,
    ) -> CanonicalCheckpoints {
        CanonicalCheckpoints {
            source: self.canonical_commits(dispatch),
            claim,
        }
    }
    /// Internal identity resolution. A locator grants neither tenant access nor a claim.
    pub async fn find_session(&self, id: &str) -> Result<Option<SessionStorage>> {
        let managed = self.component_store("managed_session")?;
        let dispatch = self.component_store("dispatch")?;
        let object = if let Some(object) = managed.load(id).await.map_err(repository_error)? {
            Some(object)
        } else if let Some(object) = dispatch.load(id).await.map_err(repository_error)? {
            Some(object)
        } else {
            dispatch
                .locate_record(&format!("thread:{id}"))
                .await
                .map_err(repository_error)?
        };
        object
            .map(|object| {
                SessionKey::new(object.owner_scope, object.session_id).map(|key| self.session(key))
            })
            .transpose()
    }
}
impl CanonicalRunCommits {
    /// Attach staged payloads at the same publication point as the canonical
    /// operation, receipt and checkpoint retirement. No mirror claim is created.
    pub async fn commit_with_blobs(
        &self,
        command: ClaimedCommitCommand,
        artifacts: &[crate::BlobRef],
    ) -> Result<CommitReceipt> {
        let (session, authority) = self.authority(&command.claim).await?;
        if command.operation.commit.run_id() != authority.observation.request.run_id()
            || command.operation.commit.thread_id != *authority.observation.request.thread_id()
            || command.operation.operation_id.run_id != *authority.observation.request.run_id()
        {
            return Err(Error::Invalid(
                "commit does not match the claimed Dispatch".into(),
            ));
        }
        awaken_agent_contract::thread::commit::operation::require_commit_operation_payload_hash(
            &command.operation,
        )
        .map_err(|error| Error::Identity(error.to_string()))?;
        session
            .commit_authorized(
                &command.claim,
                command.operation,
                artifacts,
                Some(&authority.condition),
            )
            .await
    }
    pub(crate) async fn authority(
        &self,
        claim: &RunClaim,
    ) -> Result<(SessionStorage, ConditionalClaimAuthority)> {
        let authority = match self
            .dispatch
            .acquire_commit_authority(claim)
            .await
            .map_err(|e| Error::Storage(e.to_string()))?
        {
            Some(ClaimAuthority::Conditional(authority)) => authority,
            Some(ClaimAuthority::Locked(guard)) => {
                guard.release().await.map_err(Error::Storage)?;
                return Err(Error::Invalid(
                    "S3 requires its canonical conditional Dispatch authority".into(),
                ));
            }
            None => return Err(Error::Fenced),
        };
        let object = self
            .storage
            .component_store("dispatch")?
            .locate_record(&format!("run:{}", claim.run_id.0))
            .await
            .map_err(repository_error)?
            .ok_or_else(|| Error::Corrupt("accepted Run route absent".into()))?;
        if authority.observation.request.run_id() != &claim.run_id
            || authority.observation.request.session_thread_id().0 != object.session_id
            || authority
                .observation
                .request
                .execution_scope
                .as_ref()
                .is_some_and(|scope| scope.0.0 != object.owner_scope)
        {
            return Err(Error::Fenced);
        }
        let session = self
            .storage
            .session(SessionKey::new(object.owner_scope, object.session_id)?);
        Ok((session, authority))
    }
}
#[async_trait::async_trait]
impl ClaimedRunCommit<RunError> for CanonicalRunCommits {
    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> std::result::Result<CommitReceipt, RunError> {
        self.commit_with_blobs(command, &[])
            .await
            .map_err(crate::ports::run_error)
    }
}
#[async_trait::async_trait]
impl ClaimedRunCommit<CommitError> for CanonicalRunCommits {
    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> std::result::Result<CommitReceipt, CommitError> {
        <Self as ClaimedRunCommit<RunError>>::commit_operation(self, command)
            .await
            .map_err(|error| CommitError::Rejected(error.to_string()))
    }
}
/// Constructed from one exact claim after scheduling. Reads and writes never
/// substitute a successor's current claim for the caller's captured grant.
pub struct CanonicalCheckpoints {
    source: CanonicalRunCommits,
    claim: RunClaim,
}
#[async_trait::async_trait]
impl StreamCheckpointStore for CanonicalCheckpoints {
    async fn get(&self, run: &str) -> Observed<StreamCheckpoint> {
        if run != self.claim.run_id.0 {
            return Observed::Unknown(UnknownCause::Fenced("checkpoint Run mismatch".into()));
        }
        let result = async {
            let (session, authority) = self.source.authority(&self.claim).await?;
            let (head, _) = session.inner.load_head(&session.key).await?;
            session
                .inner
                .validate_claim_condition(&head, &self.claim, &authority.condition, false)
                .await?;
            Ok(session.streams().checkpoint_at(&head, run).await)
        }
        .await;
        match result {
            Ok(observed) => observed,
            Err(Error::Fenced) => {
                Observed::Unknown(UnknownCause::Fenced("checkpoint claim is stale".into()))
            }
            Err(error) => Observed::Unknown(UnknownCause::Storage(error.to_string())),
        }
    }
    async fn put(
        &self,
        checkpoint: StreamCheckpoint,
    ) -> std::result::Result<(), StreamCheckpointError> {
        let (session, authority) = self
            .source
            .authority(&self.claim)
            .await
            .map_err(checkpoint_error)?;
        session
            .streams()
            .put_checkpoint_conditionally(&self.claim, checkpoint, &authority.condition)
            .await
            .map_err(checkpoint_error)
    }
    async fn delete(&self, run: &str) -> std::result::Result<(), StreamCheckpointError> {
        let (session, authority) = self
            .source
            .authority(&self.claim)
            .await
            .map_err(checkpoint_error)?;
        session
            .streams()
            .delete_checkpoint_conditionally(&self.claim, run, &authority.condition)
            .await
            .map_err(checkpoint_error)
    }
}
fn checkpoint_error(error: Error) -> StreamCheckpointError {
    match error {
        Error::Fenced => StreamCheckpointError::Fenced(error.to_string()),
        error => StreamCheckpointError::Storage(error.to_string()),
    }
}
fn repository_error(error: awaken_object_storage::PublicationError) -> Error {
    use awaken_object_storage::PublicationError;
    match error {
        PublicationError::Corrupt(detail) => Error::Corrupt(detail),
        PublicationError::NotApplied(detail) => Error::Conflict(detail),
        PublicationError::InvalidMutation(detail) => Error::Invalid(detail),
        error => Error::Storage(error.to_string()),
    }
}

pub struct ReadOnlyCheckpoints {
    storage: S3Storage,
    session_id: String,
}
impl S3Storage {
    pub fn read_only_checkpoints(&self, session_id: &str) -> ReadOnlyCheckpoints {
        ReadOnlyCheckpoints {
            storage: self.clone(),
            session_id: session_id.into(),
        }
    }
}
#[async_trait::async_trait]
impl StreamCheckpointStore for ReadOnlyCheckpoints {
    async fn get(&self, run: &str) -> Observed<StreamCheckpoint> {
        let result: Result<Observed<StreamCheckpoint>> = async {
            let session = self
                .storage
                .find_session(&self.session_id)
                .await?
                .ok_or(Error::NotFound)?;
            Ok(session.streams().checkpoint(run).await)
        }
        .await;
        match result {
            Ok(observed) => observed,
            Err(error) => Observed::Unknown(UnknownCause::Storage(error.to_string())),
        }
    }
    async fn put(
        &self,
        _checkpoint: StreamCheckpoint,
    ) -> std::result::Result<(), StreamCheckpointError> {
        Err(StreamCheckpointError::Fenced(
            "an exact claim is required for checkpoint writes".into(),
        ))
    }
    async fn delete(&self, _run: &str) -> std::result::Result<(), StreamCheckpointError> {
        Err(StreamCheckpointError::Fenced(
            "an exact claim is required for checkpoint deletion".into(),
        ))
    }
}

impl CanonicalRunCommits {
    pub async fn append_progress_batch(
        &self,
        claim: &RunClaim,
        batch: awaken_dispatch_contract::ProgressBatch,
    ) -> std::result::Result<
        awaken_dispatch_contract::ProgressReceipt,
        awaken_dispatch_contract::ProgressError,
    > {
        use awaken_dispatch_contract::ProgressReceipt;
        batch.validate(claim)?;
        let thread = batch.response.thread_id.clone();
        let (cursor, bytes) =
            crate::SessionStreams::encode_progress(claim, batch).map_err(progress_error)?;
        let publication = async {
            let (session, authority) = self.authority(claim).await?;
            if authority.observation.request.thread_id() != &thread {
                return Err(Error::Invalid("progress Thread mismatch".into()));
            }
            session
                .streams()
                .append_media_stream_conditionally(
                    claim,
                    cursor.clone(),
                    bytes.clone(),
                    crate::progress::OBSERVATIONS_CONTENT_TYPE,
                    &authority.condition,
                )
                .await
        }
        .await;
        let published = match publication {
            Err(
                error @ (Error::Fenced
                | Error::Conflict(_)
                | Error::OutcomeUnknown(_)
                | Error::Storage(_)),
            ) => {
                // A competing retry may have published this exact batch before
                // the claim was fenced or the step closed. Observe the durable
                // receipt after the error. This read never grants write access.
                self.progress_receipt(&cursor, &bytes).await?.ok_or(error)
            }
            other => other,
        }
        .map_err(progress_error)?;
        Ok(ProgressReceipt {
            next_batch: published.next_frame,
        })
    }

    async fn progress_receipt(
        &self,
        cursor: &crate::StreamCursor,
        bytes: &bytes::Bytes,
    ) -> std::result::Result<Option<crate::StreamCursor>, awaken_dispatch_contract::ProgressError>
    {
        let object = self
            .storage
            .component_store("dispatch")
            .map_err(progress_error)?
            .locate_record(&format!("run:{}", cursor.run))
            .await
            .map_err(|e| progress_error(repository_error(e)))?;
        let Some(object) = object else {
            return Ok(None);
        };
        let key = SessionKey::new(object.owner_scope, object.session_id).map_err(progress_error)?;
        self.storage
            .session(key)
            .streams()
            .replay_progress_receipt(cursor, bytes)
            .await
            .map_err(progress_error)
    }
}

fn progress_error(error: Error) -> awaken_dispatch_contract::ProgressError {
    use awaken_dispatch_contract::ProgressError;
    match error {
        Error::Fenced => ProgressError::Fenced,
        Error::Conflict(_) => ProgressError::Conflict,
        Error::Invalid(_) => ProgressError::Invalid,
        error => {
            tracing::warn!(%error, "could not confirm durable progress");
            ProgressError::Unavailable
        }
    }
}
