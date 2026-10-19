// @formal surface requirement=platform.resilience.failure_safety
// @formal reason: Neutral failure-disposition vocabulary only: Transient/Quota/Permanent/StopRouting
// @formal reason: and the Classify port that inference and sandbox failures implement. It holds no retry
// @formal reason: state and performs no effect; the bounded retry kernel and its Kani proofs stay with
// @formal reason: their owner in awaken-runtime-contract. This is a product-requirement classification,
// @formal reason: not a proof claim.
//! Failure disposition vocabulary: what a caller should DO with a failure.
//!
//! A neutral value object shared by every contract whose failures feed retry,
//! failover or cooldown decisions. It lives in the lowest contract so a
//! failure classifier never has to depend on the runtime contract (and deepen
//! the contract graph) just to name its disposition.

use std::time::Duration;

/// The failover disposition of an inference failure.
///
/// Four states, projecting a finer failure taxonomy onto the distinction
/// a router acts on:
/// - [`Transient`](Self::Transient): retrying the *same* binding may succeed.
/// - [`Quota`](Self::Quota): a rate/quota signal *against this credential-identity*
///   — prefer another candidate and cool this one until `retry_after`.
/// - [`Permanent`](Self::Permanent): retrying the identical request cannot succeed;
///   only a different binding or an authored change can.
/// - [`StopRouting`](Self::StopRouting): the application prerequisite or caller
///   owns the failure; preserve the selected binding and its health state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// A transport blip, timeout, or provider overload — the same call may work
    /// on retry.
    Transient,
    /// A rate-limit / quota exhaustion attributable to the credential-identity.
    /// `retry_after` is the provider's hint for when it may clear (`None` when
    /// unknown), used as the cooldown deadline.
    Quota { retry_after: Option<Duration> },
    /// A permanent rejection of this request shape (bad binding, context overflow,
    /// auth failure, missing model, content filter).
    Permanent,
    /// Stop candidate/credential routing. The failing source still owns its
    /// same-call retry policy; this does not grant physical re-entry.
    StopRouting,
}

/// A disposition answers the routing question only. Whether the *same* call
/// may be retried is owned by the failing source's own retry decision (for
/// inference, `awaken_runtime_contract::resilience::RetryPolicy::decide`): a
/// quota signal can both cool this identity and clear in place after the
/// provider's pause, so a second in-place retry predicate here could only
/// disagree with that decision.
impl Disposition {
    /// Whether a router should prefer failing over to another candidate rather
    /// than retrying in place. True for a quota signal (cool this identity) and a
    /// permanent error (this binding cannot serve the request).
    #[must_use]
    pub fn prefers_failover(self) -> bool {
        matches!(self, Disposition::Quota { .. } | Disposition::Permanent)
    }

    /// The cooldown deadline hint carried by a [`Quota`](Self::Quota) signal; `None`
    /// for every other disposition.
    #[must_use]
    pub fn retry_after(self) -> Option<Duration> {
        match self {
            Disposition::Quota { retry_after } => retry_after,
            _ => None,
        }
    }
}

/// Project a failure onto its routing [`Disposition`]. Implemented for the
/// runtime's inference error and the sandbox error; other fallible sources can opt in without the
/// router learning their error type.
pub trait Classify {
    /// The failover disposition of this failure.
    fn disposition(&self) -> Disposition;
}
