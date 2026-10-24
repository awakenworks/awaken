// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: The checkpoint adapter retains the canonical dispatch commit-epoch guard across local
// @formal reason: reads/writes/deletes, or delegates to the claim-bound upstream checkpoint operations.
// @formal reason: Replacement must fence stale mutation. The checkpoint_fence decision-table test and
// @formal reason: transport E2E own executable adapter coverage; this classification does not promote
// @formal reason: asynchronous adapter wiring to a production-kernel proof.
//! Claim-fenced adapter for interrupted-stream checkpoints.
//!
//! A stale or unverifiable claim receives an explicit fenced error and cannot
//! overwrite/delete the current attempt's checkpoint. The dispatch epoch guard
//! is held across the underlying operation, preventing reclaim from racing the
//! write.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::observation::{Observed, UnknownCause};
use awaken_agent_contract::stream::checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};

use awaken_dispatch_contract::{
    ClaimedCheckpointTransport, DispatchCommitAuthority, RunClaim, SettleOutcome,
};

pub struct FencedStreamCheckpointStore {
    authority: CheckpointAuthority,
    claim: RunClaim,
}
enum CheckpointAuthority {
    Local {
        inner: Option<Arc<dyn StreamCheckpointStore>>,
        dispatch: Arc<dyn DispatchCommitAuthority>,
    },
    Remote(Arc<dyn ClaimedCheckpointTransport>),
}
impl FencedStreamCheckpointStore {
    #[must_use]
    pub fn new(
        inner: Option<Arc<dyn StreamCheckpointStore>>,
        dispatch: Arc<dyn DispatchCommitAuthority>,
        claim: RunClaim,
    ) -> Self {
        Self {
            authority: CheckpointAuthority::Local { inner, dispatch },
            claim,
        }
    }
    #[must_use]
    pub fn remote(transport: Arc<dyn ClaimedCheckpointTransport>, claim: RunClaim) -> Self {
        Self {
            authority: CheckpointAuthority::Remote(transport),
            claim,
        }
    }
    fn verify_run(&self, run: &str) -> Result<(), StreamCheckpointError> {
        if run != self.claim.run_id.0 {
            Err(StreamCheckpointError::Fenced(
                "checkpoint run does not match claim".into(),
            ))
        } else {
            Ok(())
        }
    }
}
#[async_trait]
impl StreamCheckpointStore for FencedStreamCheckpointStore {
    async fn get(&self, run_id: &str) -> Observed<StreamCheckpoint> {
        if let Err(error) = self.verify_run(run_id) {
            return Observed::Unknown(UnknownCause::Fenced(error.to_string()));
        }
        match &self.authority {
            CheckpointAuthority::Local { inner, dispatch } => {
                match dispatch.lock_commit_epoch(&self.claim).await {
                    Ok(Some(_guard)) => match inner {
                        Some(inner) => inner.get(run_id).await,
                        None => Observed::Absent,
                    },
                    Ok(None) => Observed::Unknown(UnknownCause::Fenced(
                        "checkpoint claim is no longer current".into(),
                    )),
                    Err(error) => Observed::Unknown(UnknownCause::Storage(error.to_string())),
                }
            }
            CheckpointAuthority::Remote(remote) => {
                match remote.load_stream_checkpoint(&self.claim).await {
                    Ok(Some(checkpoint)) => Observed::Present(checkpoint),
                    Ok(None) => Observed::Absent,
                    Err(error) => Observed::Unknown(UnknownCause::Storage(error.to_string())),
                }
            }
        }
    }
    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), StreamCheckpointError> {
        self.verify_run(&checkpoint.run_id)?;
        match &self.authority {
            CheckpointAuthority::Local { inner, dispatch } => {
                let guard = dispatch
                    .lock_commit_epoch(&self.claim)
                    .await
                    .map_err(|e| StreamCheckpointError::Storage(e.to_string()))?
                    .ok_or_else(|| {
                        StreamCheckpointError::Fenced(
                            "checkpoint claim is no longer current".into(),
                        )
                    })?;
                let result = match inner {
                    Some(inner) => inner.put(checkpoint).await,
                    None => Ok(()),
                };
                guard
                    .release_after(result)
                    .await
                    .map_err(|e| StreamCheckpointError::Storage(e.to_string()))?
            }
            CheckpointAuthority::Remote(remote) => settle_result(
                remote
                    .put_stream_checkpoint(&self.claim, checkpoint)
                    .await
                    .map_err(|e| StreamCheckpointError::Storage(e.to_string()))?,
            ),
        }
    }
    async fn delete(&self, run_id: &str) -> Result<(), StreamCheckpointError> {
        self.verify_run(run_id)?;
        match &self.authority {
            CheckpointAuthority::Local { inner, dispatch } => {
                let guard = dispatch
                    .lock_commit_epoch(&self.claim)
                    .await
                    .map_err(|e| StreamCheckpointError::Storage(e.to_string()))?
                    .ok_or_else(|| {
                        StreamCheckpointError::Fenced(
                            "checkpoint claim is no longer current".into(),
                        )
                    })?;
                let result = match inner {
                    Some(inner) => inner.delete(run_id).await,
                    None => Ok(()),
                };
                guard
                    .release_after(result)
                    .await
                    .map_err(|e| StreamCheckpointError::Storage(e.to_string()))?
            }
            CheckpointAuthority::Remote(remote) => settle_result(
                remote
                    .delete_stream_checkpoint(&self.claim)
                    .await
                    .map_err(|e| StreamCheckpointError::Storage(e.to_string()))?,
            ),
        }
    }
}
fn settle_result(outcome: SettleOutcome) -> Result<(), StreamCheckpointError> {
    match outcome {
        SettleOutcome::Applied => Ok(()),
        SettleOutcome::Fenced => Err(StreamCheckpointError::Fenced(
            "checkpoint claim is no longer current".into(),
        )),
    }
}
