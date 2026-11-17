// @formal surface requirement=protocol.ui_adapters.stream_projection
// @formal reason: Carries verified attribution on best-effort preview delivery.
// @formal reason: The publishing adapter retains lease admission; metadata grants no authority.
//! Claim-bound publication of best-effort live Run progress.

use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::event::Observation as StreamObservation;
use awaken_agent_contract::stream::sink::Error as StreamError;
use awaken_lease_contract::fence::Epoch;

use awaken_worker_contract::RunClaim;

tokio::task_local! {
    static PREVIEW_CLAIM_EPOCH: Option<Epoch>;
}

/// Carry verified transport attribution through the existing SDK StreamSink.
/// This is in-process observation metadata and is never claim authority.
pub async fn with_preview_claim_epoch<F: Future>(claim: &RunClaim, publication: F) -> F::Output {
    PREVIEW_CLAIM_EPOCH
        .scope(Epoch::from_persisted(claim.epoch), publication)
        .await
}

pub fn preview_claim_epoch() -> Option<Epoch> {
    PREVIEW_CLAIM_EPOCH.try_with(|epoch| *epoch).ok().flatten()
}

#[async_trait::async_trait]
pub trait ClaimedStreamPublisher: Send + Sync {
    async fn publish(&self, claim: &RunClaim, event: StreamEvent) -> Result<(), StreamError>;

    /// Publish one context-bearing observation through the same claim-fenced
    /// live channel. Existing publishers remain source-compatible and lower to
    /// their historical Event path; context-aware transports override this.
    async fn publish_observation(
        &self,
        claim: &RunClaim,
        observation: StreamObservation,
    ) -> Result<(), StreamError> {
        self.publish(claim, observation.event).await
    }
}
