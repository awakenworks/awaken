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

use crate::{DispatchQueue, RunClaim, SettleOutcome};

pub struct FencedStreamCheckpointStore {
    inner: Option<Arc<dyn StreamCheckpointStore>>,
    dispatch: Arc<dyn DispatchQueue>,
    claim: RunClaim,
}

impl FencedStreamCheckpointStore {
    #[must_use]
    pub fn new(
        inner: Option<Arc<dyn StreamCheckpointStore>>,
        dispatch: Arc<dyn DispatchQueue>,
        claim: RunClaim,
    ) -> Self {
        Self {
            inner,
            dispatch,
            claim,
        }
    }
}

#[async_trait]
impl StreamCheckpointStore for FencedStreamCheckpointStore {
    async fn get(&self, run_id: &str) -> Observed<StreamCheckpoint> {
        if run_id != self.claim.run_id.0 {
            return Observed::Unknown(UnknownCause::Fenced(
                "checkpoint run does not match claim".into(),
            ));
        }
        match self.dispatch.lock_commit_epoch(&self.claim).await {
            Ok(Some(_guard)) => match &self.inner {
                Some(inner) => inner.get(run_id).await,
                // No local store was ever wired behind this guard, so nothing
                // was written here to find. This is a finding, not ignorance.
                None => Observed::Absent,
            },
            Ok(None) => Observed::Unknown(UnknownCause::Fenced(
                "checkpoint claim is no longer current".into(),
            )),
            // Fall back to the authority over the transport. Its Result is
            // translated here, once and explicitly: a transport failure is
            // ignorance about the remote checkpoint, never its absence.
            Err(_) => match self.dispatch.claimed_checkpoint_transport() {
                Some(remote) => match remote.load_stream_checkpoint(&self.claim).await {
                    Ok(Some(checkpoint)) => Observed::Present(checkpoint),
                    Ok(None) => Observed::Absent,
                    Err(error) => Observed::Unknown(UnknownCause::Storage(error.to_string())),
                },
                None => Observed::Unknown(UnknownCause::Storage(
                    "checkpoint composition has neither a local epoch guard nor a remote transport"
                        .into(),
                )),
            },
        }
    }

    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), StreamCheckpointError> {
        if checkpoint.run_id != self.claim.run_id.0 {
            return Err(StreamCheckpointError::Fenced(
                "checkpoint run does not match claim".into(),
            ));
        }
        match self.dispatch.lock_commit_epoch(&self.claim).await {
            Ok(Some(_guard)) => {
                if let Some(inner) = &self.inner {
                    inner.put(checkpoint).await
                } else {
                    Ok(())
                }
            }
            Ok(None) => Err(StreamCheckpointError::Fenced(
                "checkpoint claim is no longer current".into(),
            )),
            Err(_) => self
                .dispatch
                .claimed_checkpoint_transport()
                .ok_or_else(|| {
                    StreamCheckpointError::Storage(
                        "checkpoint composition has no remote transport".into(),
                    )
                })?
                .put_stream_checkpoint(&self.claim, checkpoint)
                .await
                .map_err(|error| StreamCheckpointError::Storage(error.to_string()))
                .and_then(settle_result),
        }
    }

    async fn delete(&self, run_id: &str) -> Result<(), StreamCheckpointError> {
        if run_id != self.claim.run_id.0 {
            return Err(StreamCheckpointError::Fenced(
                "checkpoint run does not match claim".into(),
            ));
        }
        match self.dispatch.lock_commit_epoch(&self.claim).await {
            Ok(Some(_guard)) => {
                if let Some(inner) = &self.inner {
                    inner.delete(run_id).await
                } else {
                    Ok(())
                }
            }
            Ok(None) => Err(StreamCheckpointError::Fenced(
                "checkpoint claim is no longer current".into(),
            )),
            Err(_) => self
                .dispatch
                .claimed_checkpoint_transport()
                .ok_or_else(|| {
                    StreamCheckpointError::Storage(
                        "checkpoint composition has no remote transport".into(),
                    )
                })?
                .delete_stream_checkpoint(&self.claim)
                .await
                .map_err(|error| StreamCheckpointError::Storage(error.to_string()))
                .and_then(settle_result),
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
