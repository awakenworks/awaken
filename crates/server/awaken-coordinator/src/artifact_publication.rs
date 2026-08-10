//! Coordinator-local claim fencing for Runtime artifact publication.

use std::sync::Arc;

use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublicationReceipt, ArtifactPublisher,
};
use awaken_run_ingress::{AnyDispatchStore, DispatchQueue, RunClaim};

/// Decorates the canonical Resources publisher with the same exact-epoch fence
/// used by the remote Worker endpoint. The Runtime Host therefore has one
/// topology-independent publication contract.
pub struct ClaimFencedArtifactPublisher {
    inner: Arc<dyn ArtifactPublisher<RunClaim>>,
    dispatch: Arc<AnyDispatchStore>,
}

impl ClaimFencedArtifactPublisher {
    #[must_use]
    pub fn new(
        inner: Arc<dyn ArtifactPublisher<RunClaim>>,
        dispatch: Arc<AnyDispatchStore>,
    ) -> Self {
        Self { inner, dispatch }
    }
}

#[async_trait::async_trait]
impl ArtifactPublisher<RunClaim> for ClaimFencedArtifactPublisher {
    async fn publish(
        &self,
        publication: ArtifactPublication<RunClaim>,
    ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError> {
        let _guard = if let Some(claim) = publication.fence.as_ref() {
            Some(
                self.dispatch
                    .lock_commit_epoch(claim)
                    .await
                    .map_err(|error| ArtifactPublicationError::new(error.to_string()))?
                    .ok_or_else(|| ArtifactPublicationError::new("dispatch claim is stale"))?,
            )
        } else {
            None
        };
        self.inner.publish(publication).await
    }
}
