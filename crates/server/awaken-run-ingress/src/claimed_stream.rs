//! Claim-bound publication of best-effort live Run progress.
//!
//! The runtime still emits the one neutral [`StreamEvent`] vocabulary through its
//! ordinary [`StreamSink`] port. A durable worker adds the exact [`RunClaim`] only
//! at the transport boundary, so a remote Coordinator can fence the observation
//! without leaking dispatch epochs into the runtime/domain contract.

use std::sync::Arc;

use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::event::Observation as StreamObservation;
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
    async fn publish(&self, claim: &RunClaim, event: StreamEvent) -> Result<(), StreamError> {
        self.publish_observation(claim, event.into()).await
    }

    async fn publish_observation(
        &self,
        _claim: &RunClaim,
        observation: StreamObservation,
    ) -> Result<(), StreamError> {
        self.sink.send_observation(observation).await
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

    async fn publish_observation(&self, observation: StreamObservation) -> Result<(), StreamError> {
        if observation.event.run_id != self.claim.run_id {
            return Err(StreamError::Closed);
        }
        self.publisher
            .publish_observation(&self.claim, observation)
            .await
    }
}

#[async_trait::async_trait]
impl StreamSink for ClaimBoundStreamSink {
    async fn send(&self, event: StreamEvent) -> Result<(), StreamError> {
        self.publish_observation(event.into()).await
    }

    async fn send_observation(&self, observation: StreamObservation) -> Result<(), StreamError> {
        self.publish_observation(observation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::event::{AgentEvent, Delta};
    use awaken_store_inmem::MemoryStreamSink;

    /// Cause/effect and decision table for the claim adapter.
    /// Causes: C1 the Runtime event names the claimed Run; C2 it names another
    /// Run; C3 it carries an exact assistant-response coordinate. Effects: E1
    /// forward exactly once through the canonical StreamSink; E2 reject without
    /// publishing; E3 preserve C3 through that same port. Constraint: the claim
    /// is observation authority only and never changes committed state.
    ///
    /// | Rule | Run id | coordinate | Effect |
    /// |---|---|---|---|
    /// | R1 | exact | absent | E1 |
    /// | R2 | foreign | any | E2 |
    /// | R3 | exact | exact | E1+E3 |
    /// Decision rule: R1-R3 cover the only exact/foreign Run and
    /// absent/present coordinate partitions admitted by this claim-bound port.
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
        sink.send_observation(StreamObservation::assistant_delta(
            RunId("run-a".into()),
            awaken_agent_contract::agent::thread::Id("thread-a".into()),
            2,
            3,
            AgentEvent::Delta(Delta::TextDelta { delta: "y".into() }),
        ))
        .await
        .expect("R3 forwards exact context");
        assert_eq!(downstream.events().len(), 2);
        assert_eq!(
            downstream.observations()[1]
                .assistant_response
                .as_ref()
                .map(|coordinate| (
                    coordinate.thread_id.0.as_str(),
                    coordinate.step,
                    coordinate.response,
                )),
            Some(("thread-a", 2, 3)),
            "R3/E3"
        );
    }
}
