//! Claim-fenced adapter for best-effort interrupted-stream checkpoints.
//!
//! The checkpoint contract intentionally returns no errors. This adapter therefore
//! fails closed: a stale or unverifiable claim observes no checkpoint and cannot
//! overwrite/delete the current attempt's checkpoint. The dispatch epoch guard is
//! held across the underlying operation, preventing reclaim from racing the write.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};

use crate::{DispatchQueue, RunClaim};

pub struct FencedStreamCheckpointStore {
    inner: Arc<dyn StreamCheckpointStore>,
    dispatch: Arc<dyn DispatchQueue>,
    claim: RunClaim,
}

impl FencedStreamCheckpointStore {
    #[must_use]
    pub fn new(
        inner: Arc<dyn StreamCheckpointStore>,
        dispatch: Arc<dyn DispatchQueue>,
        claim: RunClaim,
    ) -> Self {
        Self {
            inner,
            dispatch,
            claim,
        }
    }

    async fn guard(&self) -> Option<crate::CommitEpochGuard> {
        self.dispatch
            .lock_commit_epoch(&self.claim)
            .await
            .ok()
            .flatten()
    }
}

#[async_trait]
impl StreamCheckpointStore for FencedStreamCheckpointStore {
    async fn get(&self, run_id: &str) -> Option<StreamCheckpoint> {
        if run_id != self.claim.run_id.0 {
            return None;
        }
        let _guard = self.guard().await?;
        self.inner.get(run_id).await
    }

    async fn put(&self, checkpoint: StreamCheckpoint) {
        if checkpoint.run_id != self.claim.run_id.0 {
            return;
        }
        let Some(_guard) = self.guard().await else {
            return;
        };
        self.inner.put(checkpoint).await;
    }

    async fn delete(&self, run_id: &str) {
        if run_id != self.claim.run_id.0 {
            return;
        }
        let Some(_guard) = self.guard().await else {
            return;
        };
        self.inner.delete(run_id).await;
    }
}
