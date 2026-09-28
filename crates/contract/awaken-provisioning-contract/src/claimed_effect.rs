// @formal entrypoint claimed_effect.retention_runs_once_for_an_unsettled_drop
//! A physical effect that cannot run before someone is on record for it.
//!
//! # Why this exists as a type
//!
//! The recurring defect is not a missing cleanup call; it is an ordering that
//! nothing enforces. An `async fn` obtains a fence, then `.await`s a call that
//! creates a sandbox, container, mount or process, with the in-flight future
//! held only in a local. A task cancelled mid-await leaves the resource on the
//! host and no record says whose it is, so no later disposal, adoption or
//! reconciliation path can find it. A scanner can freeze that shape; only the
//! type system can make it unrepresentable.
//!
//! [`ClaimedEffect`] is that type. Its only constructor takes an
//! [`EffectClaim`] **by value**, and an `EffectClaim` can only be minted by
//! someone already holding the thing that does the recording, so the record
//! exists before the future is built and therefore before any poll can start
//! the effect. Dropping the effect while it is unsettled runs the claim's
//! retention; completing it — `Ok` or `Err` — settles the claim, and a settled
//! claim never retains again.
//!
//! A port that returns `ClaimedEffect` *synchronously* pushes the obligation
//! onto the implementor: a provider that cannot say who is responsible for its
//! effect fails to compile rather than leaving the question to a reviewer.

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::SandboxError;

/// Whether the claim still stands for an effect that may be in flight.
///
/// Extracted from the two `Drop` bodies that read it so the retention decision
/// is one total function a proof can exhaust, rather than a flag interpreted
/// twice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClaimDisposition {
    /// The effect may have started and nobody has accounted for it yet.
    Unsettled,
    /// The effect's outcome is accounted for; nothing further is owed.
    Settled,
}

/// Everything that can end a claim's life. There is no fourth case: an effect
/// is completed, deliberately abandoned, or dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClaimTransition {
    /// The effect ran to completion. `ok` distinguishes success from failure
    /// only so a proof can show the distinction does not matter: a failed
    /// effect is equally no longer in flight, and its own error path owns
    /// whatever it half-created.
    Completed { ok: bool },
    /// The holder states that no effect was started.
    Abandoned,
    /// The holder went away without saying which of the two happened.
    Dropped,
}

/// The claim state machine: `(next disposition, whether retention runs now)`.
///
/// Retention is the sole output that can reach the host, so it is returned
/// rather than performed here — the decision stays total and pure, and the two
/// callers below are the only places that act on it.
pub(crate) const fn advance_claim(
    disposition: ClaimDisposition,
    transition: ClaimTransition,
) -> (ClaimDisposition, bool) {
    match disposition {
        // Settling is absorbing. This is what makes retention at-most-once
        // across the `into_future` handoff, where two values that both own a
        // `Drop` see the same claim in turn.
        ClaimDisposition::Settled => (ClaimDisposition::Settled, false),
        ClaimDisposition::Unsettled => match transition {
            ClaimTransition::Dropped => (ClaimDisposition::Settled, true),
            ClaimTransition::Completed { .. } | ClaimTransition::Abandoned => {
                (ClaimDisposition::Settled, false)
            }
        },
    }
}

/// Proof that a responsibility record for one physical effect exists *before*
/// the effect is allowed to run.
///
/// Minted only by the three constructors below, each of which requires holding
/// the thing that does the recording: the create request that carries the
/// resource's own stamp, the durable writer, or the in-process slot.
#[must_use = "an effect claim exists to be attached to the effect it accounts for"]
pub struct EffectClaim {
    disposition: ClaimDisposition,
    reconciler: Option<&'static str>,
    retain: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl EffectClaim {
    /// The resource is born carrying its own stamp — labels in the create
    /// request, an inode marker written by the same syscall — so a cancelled
    /// caller cannot separate the resource from its record.
    ///
    /// `reconciler` names the scan that reads that stamp back. A stamp nobody
    /// reads is not a record, so the name is mandatory: an empty one is a
    /// programming error and panics at mint time, before any effect runs.
    pub fn self_recording(reconciler: &'static str) -> Self {
        assert!(
            !reconciler.trim().is_empty(),
            "a self-recording claim must name the scan that reads its stamp back"
        );
        Self {
            disposition: ClaimDisposition::Unsettled,
            reconciler: Some(reconciler),
            retain: None,
        }
    }

    /// A durable record was written synchronously before the effect, and
    /// `reconciler` names the scan that reads it back. `retain` runs if the
    /// effect is dropped unsettled — it covers the window before the durable
    /// write, which the record itself cannot.
    pub fn durable(reconciler: &'static str, retain: impl FnOnce() + Send + 'static) -> Self {
        assert!(
            !reconciler.trim().is_empty(),
            "a durable claim must name the scan that reads its record back"
        );
        Self {
            disposition: ClaimDisposition::Unsettled,
            reconciler: Some(reconciler),
            retain: Some(Box::new(retain)),
        }
    }

    /// An in-process slot holds the unsettled effect for the next caller to
    /// fail closed on; `retain` puts it there if the effect is dropped
    /// unsettled. There is no reconciler because nothing outside this process
    /// can read an in-process slot — which is also why this is the weakest of
    /// the three and must not be used for an effect that outlives the process.
    pub fn in_process(retain: impl FnOnce() + Send + 'static) -> Self {
        Self {
            disposition: ClaimDisposition::Unsettled,
            reconciler: None,
            retain: Some(Box::new(retain)),
        }
    }

    /// The scan that reads this claim's record back, when one exists.
    #[must_use]
    pub fn reconciler(&self) -> Option<&'static str> {
        self.reconciler
    }

    /// An already-settled claim, used to leave a settled husk behind when the
    /// live claim moves on to the next owner.
    fn settled() -> Self {
        Self {
            disposition: ClaimDisposition::Settled,
            reconciler: None,
            retain: None,
        }
    }

    /// Drive the state machine and act on its single output. Taking the
    /// retention out of the `Option` in every branch is what makes retention
    /// at-most-once: a settled claim has nothing left to run.
    fn apply(&mut self, transition: ClaimTransition) {
        let (next, retain_now) = advance_claim(self.disposition, transition);
        self.disposition = next;
        let retention = self.retain.take();
        if retain_now && let Some(retention) = retention {
            retention();
        }
    }
}

/// A physical effect whose responsibility was claimed before its first poll.
///
/// Dropping it unsettled runs the claim's retention; completing it settles.
/// It is not itself a `Future`: awaiting goes through [`IntoFuture`], so the
/// value cannot be polled in place and [`Self::abandon`] stays an unambiguous
/// statement that no effect was started.
#[must_use = "a claimed effect must be awaited or explicitly abandoned"]
pub struct ClaimedEffect<'a, T> {
    claim: EffectClaim,
    effect: Option<PendingEffect<'a, T>>,
}

impl<'a, T> ClaimedEffect<'a, T> {
    /// The only constructor. The claim is taken by value, so it cannot be
    /// minted after the future is built: Rust evaluates arguments left to
    /// right, and in every shape the future is not polled until this value is
    /// awaited, which is strictly after the claim exists.
    pub fn new(
        claim: EffectClaim,
        effect: impl Future<Output = Result<T, SandboxError>> + Send + 'a,
    ) -> Self {
        Self {
            claim,
            effect: Some(Box::pin(effect)),
        }
    }

    /// The scan that reads this effect's responsibility record back, when one
    /// exists. Call sites and tests use it to state which reconciler they are
    /// relying on.
    #[must_use]
    pub fn reconciler(&self) -> Option<&'static str> {
        self.claim.reconciler()
    }

    /// Abandon before polling, settling the claim deliberately: the caller is
    /// stating that no effect was started, so retention must not run.
    pub fn abandon(mut self) {
        self.claim.apply(ClaimTransition::Abandoned);
        self.effect = None;
    }
}

impl<T> Drop for ClaimedEffect<'_, T> {
    fn drop(&mut self) {
        self.claim.apply(ClaimTransition::Dropped);
    }
}

impl<'a, T> IntoFuture for ClaimedEffect<'a, T> {
    type Output = Result<T, SandboxError>;
    type IntoFuture = ClaimedEffectInFlight<'a, T>;

    fn into_future(mut self) -> Self::IntoFuture {
        // Swapping in a settled husk hands the live claim to the in-flight
        // value without letting this value's `Drop` see it: the obligation
        // moves, it is never discharged and never duplicated. A `Drop` type
        // cannot be destructured, and this crate forbids `unsafe`, so the swap
        // is the move.
        let claim = std::mem::replace(&mut self.claim, EffectClaim::settled());
        let effect = self
            .effect
            .take()
            .expect("a claimed effect holds its future until it is awaited or abandoned");
        ClaimedEffectInFlight { claim, effect }
    }
}

/// The awaited half of a [`ClaimedEffect`]: the effect is now allowed to run,
/// The boxed effect a claim guards. Named because the claim, not the
/// future's spelling, is what a reader of this type needs to see.
type PendingEffect<'a, T> = Pin<Box<dyn Future<Output = Result<T, SandboxError>> + Send + 'a>>;

/// and this value carries the claim for exactly as long as it may be in flight.
#[must_use = "a claimed effect must be awaited or explicitly abandoned"]
pub struct ClaimedEffectInFlight<'a, T> {
    claim: EffectClaim,
    effect: Pin<Box<dyn Future<Output = Result<T, SandboxError>> + Send + 'a>>,
}

impl<T> Future for ClaimedEffectInFlight<'_, T> {
    type Output = Result<T, SandboxError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.effect.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(outcome) => {
                // Failure settles too. An effect that returned an error is no
                // longer in flight, and its own error path owns whatever it
                // half-created; retaining here would double-own it.
                this.claim.apply(ClaimTransition::Completed {
                    ok: outcome.is_ok(),
                });
                Poll::Ready(outcome)
            }
        }
    }
}

impl<T> Drop for ClaimedEffectInFlight<'_, T> {
    fn drop(&mut self) {
        self.claim.apply(ClaimTransition::Dropped);
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn disposition(settled: bool) -> ClaimDisposition {
        if settled {
            ClaimDisposition::Settled
        } else {
            ClaimDisposition::Unsettled
        }
    }

    fn transition(tag: u8, ok: bool) -> ClaimTransition {
        match tag % 3 {
            0 => ClaimTransition::Completed { ok },
            1 => ClaimTransition::Abandoned,
            _ => ClaimTransition::Dropped,
        }
    }

    /// I2, I3 and I5 over the whole transition set.
    ///
    /// The async runtime is deliberately not modelled: the only thing a poll
    /// can do to a claim is hand [`advance_claim`] one of these transitions, so
    /// exhausting the transitions exhausts the reachable behaviour. Sequencing
    /// two arbitrary transitions is what shows retention cannot run twice, and
    /// covers the `into_future` handoff where two `Drop` owners see one claim.
    /// @obligation claimed_effect.retention_runs_once_for_an_unsettled_drop
    #[kani::proof]
    fn claim_retention_runs_once_only_for_an_unsettled_drop() {
        let start = disposition(kani::any());
        let first = transition(kani::any(), kani::any());
        let (after_first, retained_first) = advance_claim(start, first);

        // I3: every transition settles, so nothing is ever left owing.
        assert!(after_first == ClaimDisposition::Settled);
        // I2 and I4: retention is exactly the unsettled drop, and never a
        // completion (Ok or Err) or an abandonment.
        assert!(
            retained_first
                == (start == ClaimDisposition::Unsettled && first == ClaimTransition::Dropped)
        );

        let second = transition(kani::any(), kani::any());
        let (after_second, retained_second) = advance_claim(after_first, second);
        // I5: a settled claim never retains, so no path runs retention twice.
        assert!(!retained_second);
        assert!(after_second == ClaimDisposition::Settled);
    }
}

#[cfg(test)]
mod tests;
