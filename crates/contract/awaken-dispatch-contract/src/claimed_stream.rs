//! Claim-bound publication of best-effort live Run progress.

use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::sink::Error as StreamError;

use crate::RunClaim;

#[async_trait::async_trait]
pub trait ClaimedStreamPublisher: Send + Sync {
    async fn publish(&self, claim: &RunClaim, event: StreamEvent) -> Result<(), StreamError>;
}
