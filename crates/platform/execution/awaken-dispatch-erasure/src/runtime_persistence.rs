// @formal surface obligation=remote_worker.committed_truth_is_coordinator_owned
// @formal reason: Revalidates the claim before returning recovery and binds checkpoints and progress to the selected authority.
//! The selected Runtime binds remote persistence to the caller's exact claim.
use crate::RuntimeAuthority;
use awaken_agent_contract::{
    observation::Observed,
    stream::{
        checkpoint::{StreamCheckpoint, StreamCheckpointStore},
        sink::Sink,
    },
    thread::read::recovery::{LoadedRunRecovery, RunRecoverySource, ThreadPrefixRef},
};
use awaken_dispatch_contract::DispatchCommitAuthority as _;
use awaken_dispatch_contract::{
    AppendRunProgress, ClaimedCheckpointTransport, DispatchError, ProgressBatch, ProgressError,
    ProgressReceipt, SettleOutcome,
};
use awaken_worker_contract::RunClaim;
use std::sync::Arc;

pub fn runtime_persistence_transport(
    authority: Arc<dyn RuntimeAuthority>,
    _live: Arc<dyn Sink>,
) -> awaken_dispatch_contract::ClaimedPersistence {
    let transport = Arc::new(RuntimePersistence { authority });
    awaken_dispatch_contract::ClaimedPersistence {
        checkpoints: transport.clone(),
        progress: transport,
    }
}
struct RuntimePersistence {
    authority: Arc<dyn RuntimeAuthority>,
}
impl RuntimePersistence {
    async fn legacy_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> Result<
        (
            awaken_dispatch_contract::CommitEpochGuard,
            Arc<dyn StreamCheckpointStore>,
        ),
        DispatchError,
    > {
        let guard = self
            .authority
            .dispatch_store()
            .lock_commit_epoch(claim)
            .await?
            .ok_or_else(|| DispatchError::Conflict("stale checkpoint claim".into()))?;
        let checkpoint = self
            .authority
            .stream_checkpoint(&guard.request().session_thread_id().0)
            .await
            .map_err(DispatchError::unavailable)?;
        Ok((guard, checkpoint))
    }
}
#[async_trait::async_trait]
impl ClaimedCheckpointTransport for RuntimePersistence {
    async fn load_stream_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> awaken_dispatch_contract::ClaimedCheckpointRead {
        let (guard, checkpoint) =
            if let Some(bound) = self.authority.bind_run_persistence(claim, None) {
                (None, bound.checkpoint)
            } else {
                let (guard, store) = self.legacy_checkpoint(claim).await?;
                (Some(guard), store)
            };
        let result = match checkpoint.get(&claim.run_id.0).await {
            Observed::Present(value) => Ok(Some(value)),
            Observed::Absent => Ok(None),
            Observed::Unknown(awaken_agent_contract::observation::UnknownCause::Fenced(detail)) => {
                Err(DispatchError::Conflict(detail))
            }
            Observed::Unknown(cause) => Err(DispatchError::unavailable(cause)),
        };
        if let Some(guard) = guard {
            guard.release().await.map_err(DispatchError::unavailable)?;
        }
        result
    }
    async fn put_stream_checkpoint(
        &self,
        claim: &RunClaim,
        checkpoint: StreamCheckpoint,
    ) -> Result<SettleOutcome, DispatchError> {
        if checkpoint.run_id != claim.run_id.0 {
            return Err(DispatchError::Conflict("checkpoint Run mismatch".into()));
        }
        let (guard, store) = if let Some(bound) = self.authority.bind_run_persistence(claim, None) {
            (None, bound.checkpoint)
        } else {
            let (guard, store) = self.legacy_checkpoint(claim).await?;
            (Some(guard), store)
        };
        let result = store
            .put(checkpoint)
            .await
            .map(|()| SettleOutcome::Applied)
            .map_err(DispatchError::unavailable);
        if let Some(guard) = guard {
            guard.release().await.map_err(DispatchError::unavailable)?;
        }
        result
    }
    async fn delete_stream_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> Result<SettleOutcome, DispatchError> {
        let (guard, store) = if let Some(bound) = self.authority.bind_run_persistence(claim, None) {
            (None, bound.checkpoint)
        } else {
            let (guard, store) = self.legacy_checkpoint(claim).await?;
            (Some(guard), store)
        };
        let result = store
            .delete(&claim.run_id.0)
            .await
            .map(|()| SettleOutcome::Applied)
            .map_err(DispatchError::unavailable);
        if let Some(guard) = guard {
            guard.release().await.map_err(DispatchError::unavailable)?;
        }
        result
    }
    async fn load_recovery(
        &self,
        claim: &RunClaim,
        work: Option<&awaken_session_application_contract::SessionWorkAuthorityReceipt>,
        cached: Option<ThreadPrefixRef>,
    ) -> Result<LoadedRunRecovery, DispatchError> {
        let dispatch = self.authority.dispatch_store();
        let guard = dispatch
            .acquire_commit_authority(claim)
            .await?
            .ok_or_else(|| DispatchError::Conflict("stale recovery claim".into()))?;
        let request = guard.request();
        // HTTP validates current Work before this trusted storage port. Here its
        // identity must still match the exact captured dispatch.
        if request.requires_session_work_authority() != work.is_some()
            || work.is_some_and(|w| {
                w.session_id().as_str() != request.session_thread_id().0
                    || w.worker_owner() != claim.owner
            })
        {
            return Err(DispatchError::Conflict(
                "recovery Work identity mismatch".into(),
            ));
        }
        let session = request.session_thread_id().clone();
        let thread = request.thread_id().clone();
        let run = request.run_id().clone();
        // Read without borrowing a second connection from the guarded pool.
        // Recheck the same epoch before returning the authoritative prefix.
        guard.release().await.map_err(DispatchError::unavailable)?;
        let source: Arc<dyn RunRecoverySource> = match self.authority.shared_recovery_source() {
            Some(source) => source,
            None => self
                .authority
                .open_commit(&session.0)
                .await
                .map_err(DispatchError::unavailable)?,
        };
        let recovery = source
            .load_recovery(&session, &thread, &run, cached)
            .await
            .map_err(DispatchError::unavailable)?;
        let guard = dispatch
            .acquire_commit_authority(claim)
            .await?
            .ok_or_else(|| DispatchError::Conflict("stale recovery claim".into()))?;
        guard.release().await.map_err(DispatchError::unavailable)?;
        Ok(recovery)
    }
}
#[async_trait::async_trait]
impl AppendRunProgress for RuntimePersistence {
    async fn append_progress_batch(
        &self,
        claim: &RunClaim,
        batch: ProgressBatch,
    ) -> Result<ProgressReceipt, ProgressError> {
        self.authority.append_progress_batch(claim, batch).await
    }
}
