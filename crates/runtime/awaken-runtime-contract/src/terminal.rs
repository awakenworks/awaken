//! Post-commit observation of terminal Runs.
//!
//! This is deliberately narrower than a lifecycle hook: an observer receives
//! committed terminal identity and cause, has no control authority, and cannot
//! change the Run result. Delivery may repeat after recovery, so implementations
//! must turn the stable identity into an idempotent intent/receipt before causing
//! non-idempotent effects.

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use std::sync::Arc;

/// The neutral committed fact delivered to terminal observers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedTerminalRun {
    pub run_id: RunId,
    pub thread_id: ThreadId,
    pub cause: EndCause,
}

/// A failed observation attempt. The committed Run remains authoritative and
/// recovery may deliver the same terminal fact again.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("terminal observer failed: {0}")]
pub struct RunTerminalObserverError(pub String);

/// Reacts to an already-committed terminal Run.
///
/// The observer owns its durable intent and receipt. `observer_id` must be
/// stable across processes so `(observer_id, run_id)` can be used as the
/// duplicate-suppression key.
#[async_trait]
pub trait RunTerminalObserver: Send + Sync {
    fn observer_id(&self) -> &str;

    async fn observe(
        &self,
        terminal: &CommittedTerminalRun,
    ) -> Result<(), RunTerminalObserverError>;
}

/// One isolated delivery failure. Callers may log/measure these failures, but
/// must not project them back into the already-committed Run result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTerminalDeliveryFailure {
    pub observer_id: String,
    pub error: String,
}

/// Deliver a terminal fact with uniform error and panic isolation across native,
/// ACP, A2A, and future Run executors.
pub async fn deliver_committed_terminal(
    observers: &[Arc<dyn RunTerminalObserver>],
    terminal: &CommittedTerminalRun,
) -> Vec<RunTerminalDeliveryFailure> {
    let mut failures = Vec::new();
    for observer in observers {
        let observer_id = observer.observer_id().to_string();
        let observer = observer.clone();
        let terminal = terminal.clone();
        match tokio::spawn(async move { observer.observe(&terminal).await }).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(RunTerminalDeliveryFailure {
                observer_id,
                error: error.to_string(),
            }),
            Err(error) => failures.push(RunTerminalDeliveryFailure {
                observer_id,
                error: if error.is_panic() {
                    "observer panicked".to_string()
                } else {
                    "observer task was cancelled".to_string()
                },
            }),
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PanickingObserver;

    #[async_trait]
    impl RunTerminalObserver for PanickingObserver {
        fn observer_id(&self) -> &str {
            "panicking-observer"
        }

        async fn observe(
            &self,
            _terminal: &CommittedTerminalRun,
        ) -> Result<(), RunTerminalObserverError> {
            panic!("expected observer panic")
        }
    }

    #[tokio::test]
    async fn a_panicking_observer_is_reported_instead_of_unwinding_delivery() {
        let terminal = CommittedTerminalRun {
            run_id: RunId("run".to_string()),
            thread_id: ThreadId("thread".to_string()),
            cause: EndCause::NaturalEnd,
        };

        let failures = deliver_committed_terminal(&[Arc::new(PanickingObserver)], &terminal).await;

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].observer_id, "panicking-observer");
        assert_eq!(failures[0].error, "observer panicked");
    }
}
