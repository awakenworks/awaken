//! Attempt and process-drain failures reported by the one dispatch executor.

use awaken_dispatch_contract::DispatchError;

/// A durable-ingress failure: either the dispatch store rejected an operation or
/// a runtime attempt failed. Kept as two arms so a queue-storage failure never
/// masquerades as a run execution failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
    #[error(transparent)]
    Execution(#[from] awaken_runtime_contract::execution::Error),
    /// Session resolution identified an absorbing application failure; the
    /// still-current Run claim must commit/settle the corresponding Run failure
    /// instead of pretending the Worker crashed.
    #[error(transparent)]
    TerminalResolution(awaken_runtime_contract::execution::Error),
    /// The claimed Run is valid, but its Session's Environment Work slot is
    /// still occupied. This is scheduling pressure, not a Worker crash or an
    /// absorbing Session failure.
    #[error("Session resolution is not ready: {0}")]
    ResolutionNotReady(String),
    /// The Run already committed terminal truth, but its exact Session
    /// settlement projection is not yet observable. Retry only this Dispatch;
    /// this is not evidence that the shared store or Worker pool is unhealthy.
    #[error("Session settlement observation is not ready: {0}")]
    SettlementObservationPending(String),
}

impl Error {
    #[must_use]
    pub fn is_terminal_resolution(&self) -> bool {
        matches!(self, Self::TerminalResolution(_))
    }

    #[must_use]
    pub fn is_resolution_not_ready(&self) -> bool {
        matches!(self, Self::ResolutionNotReady(_))
    }

    #[must_use]
    pub fn is_settlement_observation_pending(&self) -> bool {
        matches!(self, Self::SettlementObservationPending(_))
    }

    #[must_use]
    pub fn is_dispatch_conflict(&self) -> bool {
        matches!(self, Self::Dispatch(DispatchError::Conflict(_)))
    }
}

#[cfg(test)]
mod error_scope_tests {
    use super::{DispatchError, Error};

    #[test]
    fn only_an_exact_dispatch_conflict_is_a_benign_fence() {
        /* Causes: C1 transport/store reports Conflict; C2 it reports Rejected
         * or Unavailable; C3 Session settlement is locally pending. Effects:
         * E1 C1 may be absorbed as an already-fenced claim; E2 C2 retains the
         * ordinary fault path; E3 C3 uses request-local deferral. */
        assert!(Error::Dispatch(DispatchError::Conflict("stale".into())).is_dispatch_conflict());
        assert!(!Error::Dispatch(DispatchError::Rejected("invalid".into())).is_dispatch_conflict());
        assert!(
            !Error::Dispatch(DispatchError::Unavailable("store".into())).is_dispatch_conflict()
        );
        assert!(!Error::SettlementObservationPending("pending".into()).is_dispatch_conflict());
    }
}
