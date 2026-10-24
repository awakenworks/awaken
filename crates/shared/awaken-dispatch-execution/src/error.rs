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
    /// still occupied, the environment backend had no capacity for it
    /// (ADR-0092), or its provider is still provisioning within its own
    /// deadline (ADR-0015). This is no failed attempt, not a Worker crash and
    /// not an absorbing Session failure.
    #[error("Session resolution is not ready: {0}")]
    ResolutionNotReady(NotReady),
    /// The Run already committed terminal truth, but its exact Session
    /// settlement projection is not yet observable. Retry only this Dispatch;
    /// this is not evidence that the shared store or Worker pool is unhealthy.
    #[error("Session settlement observation is not ready: {0}")]
    SettlementObservationPending(String),
}

/// Why a claim's resolution is not ready, and when the waited-on owner says
/// it is worth looking again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotReady {
    reason: String,
    retry_after: Option<std::time::Duration>,
}

impl NotReady {
    /// A deferral whose cadence the pool's backoff decides.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            retry_after: None,
        }
    }

    /// A deferral whose owner bounds its wait: the pool looks again no later
    /// than `retry_after` (a provider's provisioning deadline).
    #[must_use]
    pub fn after(reason: impl Into<String>, retry_after: Option<std::time::Duration>) -> Self {
        Self {
            reason: reason.into(),
            retry_after,
        }
    }

    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    #[must_use]
    pub const fn retry_after(&self) -> Option<std::time::Duration> {
        self.retry_after
    }
}

impl std::fmt::Display for NotReady {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl From<String> for NotReady {
    fn from(reason: String) -> Self {
        Self::new(reason)
    }
}

impl From<&str> for NotReady {
    fn from(reason: &str) -> Self {
        Self::new(reason)
    }
}

/// How the pool returns a claim whose attempt it could not settle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaimRelease {
    /// Not a failed attempt (pressure, a fenced claim, a pending observation,
    /// or a queue-storage fault): defer the claim and keep its retry budget.
    Defer,
    /// A failed attempt: spend one attempt of the retry budget; the attempt
    /// that exhausts it is terminalized like a crashed one (ADR-0015).
    Retry,
}

impl Error {
    /// The one decision of how a failed claim is returned to the queue, read
    /// from this failure's class only. A terminal resolution reaches here only
    /// after execution began, where it can no longer be absorbed before entry,
    /// so it counts as a failed attempt.
    #[must_use]
    pub(crate) const fn claim_release(&self) -> ClaimRelease {
        match self {
            Self::Execution(_) | Self::TerminalResolution(_) => ClaimRelease::Retry,
            Self::Dispatch(_)
            | Self::ResolutionNotReady(_)
            | Self::SettlementObservationPending(_) => ClaimRelease::Defer,
        }
    }

    /// How long the pool waits before this claim is due again: its backoff,
    /// shortened to the waited-on owner's hint when a deferral carries one,
    /// so a deferral never sleeps past the owner's deadline.
    #[must_use]
    pub(crate) fn retry_after(&self, backoff: std::time::Duration) -> std::time::Duration {
        let hint = match self {
            Self::ResolutionNotReady(not_ready) => not_ready.retry_after(),
            _ => None,
        };
        crate::session_worker_resolver::deferral_retry_after(backoff, hint)
    }

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

    #[test]
    fn a_deferral_is_due_no_later_than_its_owners_hint() {
        // Decision table: C1 the failure class; C2 the deferral's hint vs the
        // pool's backoff. Effect: when the claim is due again.
        // | D1 | NotReady without a hint | backoff |
        // | D2 | NotReady, hint shorter than the backoff | hint |
        // | D3 | NotReady, hint longer than the backoff | backoff |
        // | D4 | a failed attempt or another deferral | backoff |
        use std::time::Duration;
        let backoff = Duration::from_secs(4);
        let execution = || awaken_runtime_contract::execution::Error::Execution("m".into());
        for (rule, error, due) in [
            ("D1", Error::ResolutionNotReady("m".into()), backoff),
            (
                "D2",
                Error::ResolutionNotReady(super::NotReady::after(
                    "m",
                    Some(Duration::from_millis(700)),
                )),
                Duration::from_millis(700),
            ),
            (
                "D3",
                Error::ResolutionNotReady(super::NotReady::after(
                    "m",
                    Some(Duration::from_secs(600)),
                )),
                backoff,
            ),
            ("D4", Error::Execution(execution()), backoff),
            (
                "D4",
                Error::SettlementObservationPending("m".into()),
                backoff,
            ),
        ] {
            assert_eq!(error.retry_after(backoff), due, "{rule}");
        }
    }

    #[test]
    fn only_a_failed_attempt_spends_the_retry_budget() {
        // Decision table: the failure class -> how the pool returns the claim.
        // | Rule | failure | release |
        // | C1 | Execution (Retry disposition) | Retry: spend one attempt |
        // | C2 | TerminalResolution after execution began | Retry |
        // | C3 | ResolutionNotReady (Defer disposition) | Defer: keep the budget |
        // | C4 | SettlementObservationPending | Defer |
        // | C5 | Dispatch fault, conflict or otherwise | Defer |
        use super::ClaimRelease;
        let execution = || awaken_runtime_contract::execution::Error::Execution("m".into());
        for (rule, error, release) in [
            ("C1", Error::Execution(execution()), ClaimRelease::Retry),
            (
                "C2",
                Error::TerminalResolution(execution()),
                ClaimRelease::Retry,
            ),
            (
                "C3",
                Error::ResolutionNotReady("m".into()),
                ClaimRelease::Defer,
            ),
            (
                "C4",
                Error::SettlementObservationPending("m".into()),
                ClaimRelease::Defer,
            ),
            (
                "C5",
                Error::Dispatch(DispatchError::Conflict("m".into())),
                ClaimRelease::Defer,
            ),
            (
                "C5",
                Error::Dispatch(DispatchError::Unavailable("m".into())),
                ClaimRelease::Defer,
            ),
        ] {
            assert_eq!(error.claim_release(), release, "{rule}");
        }
    }
}
