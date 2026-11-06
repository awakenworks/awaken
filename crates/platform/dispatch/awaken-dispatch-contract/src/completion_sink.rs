// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: One notification-only settlement port; committed Run truth remains the outcome and
// @formal reason: this port owns no queue, claim or delivery decision.
//! The dispatch settlement notification port.

use awaken_agent_contract::agent::run::{Id as RunId, RunState};

/// Notified the instant a pool or a Worker settles a run, so a foreground submitter can await
/// its own run's completion by **event** rather than polling committed truth —
/// removing the poll-interval latency floor from the durable foreground path.
pub trait CompletionSink: Send + Sync {
    /// The pool drove `run_id` to a settled `state` (`Ended` or `Awaiting`).
    fn settled(&self, run_id: &RunId, state: &RunState);
}
