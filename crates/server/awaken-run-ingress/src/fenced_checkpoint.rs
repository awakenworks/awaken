//! Claim-fenced adapter for interrupted-stream checkpoints.
//!
//! A stale or unverifiable claim receives an explicit fenced error and cannot
//! overwrite/delete the current attempt's checkpoint. The dispatch epoch guard
//! is held across the underlying operation, preventing reclaim from racing the
//! write.

use std::sync::Arc;

use async_trait::async_trait;
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
    async fn get(&self, run_id: &str) -> Result<Option<StreamCheckpoint>, StreamCheckpointError> {
        if run_id != self.claim.run_id.0 {
            return Err(StreamCheckpointError::Fenced(
                "checkpoint run does not match claim".into(),
            ));
        }
        match self.dispatch.lock_commit_epoch(&self.claim).await {
            Ok(Some(_guard)) => match &self.inner {
                Some(inner) => inner.get(run_id).await,
                None => Ok(None),
            },
            Ok(None) => Err(StreamCheckpointError::Fenced(
                "checkpoint claim is no longer current".into(),
            )),
            Err(_) => self
                .dispatch
                .load_stream_checkpoint(&self.claim)
                .await
                .map_err(|error| StreamCheckpointError::Storage(error.to_string())),
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
