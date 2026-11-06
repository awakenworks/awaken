// @formal surface requirement=runtime.cancellation.terminal_cancellation
// @formal reason: The cause recorded before an attempt's cancellation token fires decides whether the
// @formal reason: attempt may commit a terminal Cancelled: only the Run's cancellation authority ends
// @formal reason: the Run; proof expiry, claim loss, Worker revocation and host drain abandon the attempt
// @formal reason: with no commit. The token carries no secret; the decision is an exhaustive table test.
//! Why one attempt's cooperative cancellation fired, and what that may commit.
//!
//! The cancellation token is a signal only: it tells an executor to stop at its
//! next boundary. Whether stopping also ends the Run depends on who fired it.
//! Only the Run's own cancellation authority — a durable Dispatch cancellation
//! for a claimed attempt, or the caller of an unclaimed direct Run — ends the
//! Run as `Cancelled`. A process that merely lost the right to keep driving the
//! attempt (its ownership proof expired, its claim was replaced, its Worker was
//! revoked or its host is draining) must stop without writing a terminal fact:
//! the Run itself was not cancelled, and Dispatch recovery re-claims it.
//!
//! | Recorded cause | Executor reached `Ended(Cancelled)` | Effect |
//! |---|---|---|
//! | none (untagged signal) | yes | commit `Cancelled` |
//! | `Requested` | yes | commit `Cancelled` |
//! | `ProofExpired`, `ClaimLost`, `WorkerRevoked`, `HostDrain` | yes | no commit; [`abandoned_attempt`] |
//! | any | no (another outcome) | unchanged |
//!
//! `RuntimeRunContext::admit_terminal_commit` applies this table at every
//! executor's terminal commit boundary.
//!
//! The first recorded cause wins, so a late signal cannot rewrite why the
//! attempt stopped. A signal source records its cause before it fires the
//! token ([`CancellationCauseCell::cancel`]), so an executor that observes the
//! token also observes the cause.

use std::sync::{Arc, OnceLock};

use tokio_util::sync::CancellationToken;

use crate::diagnostic::{Error, RuntimeDiagnostic, RuntimeErrorKind};

/// Diagnostic code of an attempt that stopped without ending its Run.
pub const ABANDONED_ATTEMPT_CODE: &str = "attempt_abandoned";

/// Who fired one attempt's cancellation token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CancellationCause {
    /// The Run's cancellation authority asked to cancel the Run: a durable
    /// Dispatch cancellation of a claimed attempt, or the direct caller of an
    /// unclaimed Run.
    Requested,
    /// This process could not renew its claim before the local proof window
    /// closed; the claim may still be durably current.
    ProofExpired,
    /// The claim was authoritatively replaced or released.
    ClaimLost,
    /// The Worker hosting the attempt lost its registration authority.
    WorkerRevoked,
    /// The hosting process is draining and stops its attempts.
    HostDrain,
}

impl CancellationCause {
    /// Only the Run's own cancellation authority ends the Run. Every other
    /// cause stops only this process's attempt.
    #[must_use]
    pub const fn ends_run(self) -> bool {
        matches!(self, Self::Requested)
    }
}

impl std::fmt::Display for CancellationCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Requested => "cancellation requested",
            Self::ProofExpired => "claim ownership proof expired",
            Self::ClaimLost => "claim lost",
            Self::WorkerRevoked => "worker revoked",
            Self::HostDrain => "host draining",
        })
    }
}

/// The write-once cause shared by every holder of one attempt's token.
#[derive(Clone, Debug, Default)]
pub struct CancellationCauseCell(Arc<OnceLock<CancellationCause>>);

impl CancellationCauseCell {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `cause` unless an earlier cause was recorded; returns the cause
    /// that now stands.
    pub fn record(&self, cause: CancellationCause) -> CancellationCause {
        *self.0.get_or_init(|| cause)
    }

    /// The recorded cause, if any signal source recorded one.
    #[must_use]
    pub fn recorded(&self) -> Option<CancellationCause> {
        self.0.get().copied()
    }

    /// Record `cause`, then fire `token`. This order is the contract: an
    /// executor that observes the token observes the cause.
    pub fn cancel(&self, token: &CancellationToken, cause: CancellationCause) {
        self.record(cause);
        token.cancel();
    }
}

/// One attempt's cooperative cancellation signal together with its cause cell.
///
/// Signal sources that know why they stop the attempt fire it through
/// [`cancel`](Self::cancel); `RuntimeRunContext` carries the two halves as its
/// `cancellation` and `cancellation_cause`.
#[derive(Clone, Debug, Default)]
pub struct AttemptCancellation {
    pub token: CancellationToken,
    pub cause: CancellationCauseCell,
}

impl AttemptCancellation {
    /// Record `cause` (first cause wins), then fire the token.
    pub fn cancel(&self, cause: CancellationCause) {
        self.cause.cancel(&self.token, cause);
    }
}

/// The source of an [`abandoned_attempt`] diagnostic.
#[derive(Debug, thiserror::Error)]
#[error("attempt abandoned without a terminal commit: {cause}")]
pub struct AbandonedAttempt {
    cause: CancellationCause,
}

impl AbandonedAttempt {
    #[must_use]
    pub const fn cause(&self) -> CancellationCause {
        self.cause
    }
}

/// The error an executor returns when it stops for a cause that does not end
/// the Run. It carries no terminal fact; the caller must not settle the Run.
#[must_use]
pub fn abandoned_attempt(cause: CancellationCause) -> Error {
    RuntimeDiagnostic::new(
        RuntimeErrorKind::Unavailable,
        ABANDONED_ATTEMPT_CODE,
        AbandonedAttempt { cause },
    )
    .into()
}

/// The cause of an [`abandoned_attempt`] error, read from its typed source.
#[must_use]
pub fn abandoned_attempt_cause(error: &Error) -> Option<CancellationCause> {
    use std::error::Error as _;
    let Error::Diagnostic(diagnostic) = error else {
        return None;
    };
    diagnostic
        .source()?
        .downcast_ref::<AbandonedAttempt>()
        .map(AbandonedAttempt::cause)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [CancellationCause; 5] = [
        CancellationCause::Requested,
        CancellationCause::ProofExpired,
        CancellationCause::ClaimLost,
        CancellationCause::WorkerRevoked,
        CancellationCause::HostDrain,
    ];

    #[test]
    fn only_a_requested_cancellation_ends_the_run() {
        // Rows of the module table: Requested ends the Run; every authority
        // loss stops only the attempt. Expected values are independent constants.
        let expected = [true, false, false, false, false];
        for (cause, ends) in ALL.into_iter().zip(expected) {
            assert_eq!(cause.ends_run(), ends, "{cause:?}");
        }
    }

    #[test]
    fn the_first_recorded_cause_wins_and_is_visible_once_the_token_fires() {
        // C1 a source records then fires; C2 a later source fires with another
        // cause. E1 the token is cancelled; E2 the first cause stands.
        let token = CancellationToken::new();
        let cell = CancellationCauseCell::new();
        assert_eq!(cell.recorded(), None);
        cell.cancel(&token, CancellationCause::ProofExpired);
        assert!(token.is_cancelled(), "E1");
        cell.cancel(&token, CancellationCause::Requested);
        assert_eq!(cell.recorded(), Some(CancellationCause::ProofExpired), "E2");
        assert_eq!(
            cell.record(CancellationCause::ClaimLost),
            CancellationCause::ProofExpired
        );
    }

    #[test]
    fn an_abandoned_attempt_keeps_its_cause_and_code() {
        for cause in ALL {
            let error = abandoned_attempt(cause);
            assert_eq!(abandoned_attempt_cause(&error), Some(cause), "{cause:?}");
            assert_eq!(error.code(), ABANDONED_ATTEMPT_CODE);
        }
    }

    #[test]
    fn an_ordinary_error_has_no_abandoned_cause() {
        assert_eq!(
            abandoned_attempt_cause(&Error::Execution("other".into())),
            None
        );
        assert_eq!(abandoned_attempt_cause(&Error::Indeterminate), None);
    }
}
