//! Claim-bound publication of best-effort live Run progress.
//!
//! The runtime still emits the one neutral [`StreamEvent`] vocabulary through its
//! ordinary [`StreamSink`] port. A durable worker adds the exact [`RunClaim`] only
//! at the transport boundary, so a remote Coordinator can fence the observation
//! without leaking dispatch epochs into the runtime/domain contract.

use std::sync::Arc;

use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::sink::{Error as StreamError, Sink as StreamSink};

use crate::RunClaim;
pub use awaken_run_ingress_contract::ClaimedStreamPublisher;

/// Reuses an existing process-local StreamSink behind the claim-bound worker
/// port. Local execution needs no wire fence because the dispatch worker and
/// sink share one trusted process; remote publishers consume the claim.
pub(crate) struct LocalClaimedStreamPublisher {
    sink: Arc<dyn StreamSink>,
}

impl LocalClaimedStreamPublisher {
    pub(crate) fn new(sink: Arc<dyn StreamSink>) -> Self {
        Self { sink }
    }
}

#[async_trait::async_trait]
impl ClaimedStreamPublisher for LocalClaimedStreamPublisher {
    async fn publish(&self, _claim: &RunClaim, event: StreamEvent) -> Result<(), StreamError> {
        self.sink.send(event).await
    }
}

pub(crate) struct ClaimBoundStreamSink {
    claim: RunClaim,
    publisher: Arc<dyn ClaimedStreamPublisher>,
}

impl ClaimBoundStreamSink {
    pub(crate) fn new(claim: RunClaim, publisher: Arc<dyn ClaimedStreamPublisher>) -> Self {
        Self { claim, publisher }
    }
}

#[async_trait::async_trait]
impl StreamSink for ClaimBoundStreamSink {
    async fn send(&self, event: StreamEvent) -> Result<(), StreamError> {
        if event.run_id != self.claim.run_id {
            return Err(StreamError::Closed);
        }
        self.publisher.publish(&self.claim, event).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::event::{AgentEvent, Delta};
    use awaken_store_inmem::MemoryStreamSink;

    /// Cause/effect and decision table for the claim adapter.
    /// Causes: C1 the runtime event names the claimed Run; C2 it names another
    /// Run. Effects: E1 forward exactly once through the canonical StreamSink;
    /// E2 reject without publishing. Constraint: the claim is observation
    /// authority only and never changes committed state. Rules: R1 C1=>E1;
    /// R2 C2=>E2. These two rows exhaust the identity predicate.
    #[tokio::test]
    async fn claim_bound_sink_forwards_only_its_exact_run() {
        let downstream = Arc::new(MemoryStreamSink::new());
        let publisher = Arc::new(LocalClaimedStreamPublisher::new(downstream.clone()));
        let sink = ClaimBoundStreamSink::new(
            RunClaim {
                run_id: RunId("run-a".into()),
                owner: "worker-a".into(),
                epoch: 3,
            },
            publisher,
        );
        let event = |run: &str| StreamEvent {
            run_id: RunId(run.into()),
            kind: AgentEvent::Delta(Delta::TextDelta { delta: "x".into() }),
        };

        sink.send(event("run-a")).await.expect("R1 forwards");
        assert!(sink.send(event("run-b")).await.is_err(), "R2 rejects");
        assert_eq!(downstream.events().len(), 1);
    }
}
