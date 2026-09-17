//! Claim-bound publication of best-effort live Run progress.

use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::event::Observation as StreamObservation;
use awaken_agent_contract::stream::sink::Error as StreamError;

use crate::RunClaim;

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
