//! Decision table for the claimed-effect invariants.
//!
//! Causes: C1 the effect completes `Ok`; C2 it completes `Err`; C3 the value is
//! dropped before the effect's first poll; C4 it is dropped after one poll that
//! returned `Pending`, i.e. with the effect genuinely in flight; C5 it is
//! abandoned; C6 the value is dropped a second time along the `into_future`
//! handoff, where two owners that each implement `Drop` see the same claim.
//! Effects: E1 the claim's retention runs exactly once; E2 it never runs.
//!
//! | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
//! | T1   | T  | F  | F  | F  | F  | F  | E2     |
//! | T2   | F  | T  | F  | F  | F  | F  | E2     |
//! | T3   | F  | F  | T  | F  | F  | F  | E1     |
//! | T4   | F  | F  | F  | T  | F  | F  | E1     |
//! | T5   | F  | F  | F  | F  | T  | F  | E2     |
//! | T6   | F  | F  | T  | F  | F  | T  | E1     |
//!
//! I1 is not a row because it is not an outcome: it is the ordering that makes
//! the rows meaningful, so it is asserted directly — the record exists while
//! the effect has provably not started.

use std::future::IntoFuture;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use super::*;

/// A counter standing in for whatever the retention actually records, so a run
/// is observable and a second run would be too.
fn retention() -> (Arc<AtomicUsize>, impl FnOnce() + Send + 'static) {
    let runs = Arc::new(AtomicUsize::new(0));
    let handle = Arc::clone(&runs);
    (runs, move || {
        handle.fetch_add(1, Ordering::SeqCst);
    })
}

/// Pending on the first poll, ready on the second: the only way to hold a
/// claimed effect genuinely in flight and then drop it.
struct YieldOnce {
    polled: Arc<AtomicUsize>,
    outcome: Option<Result<(), SandboxError>>,
}

impl Future for YieldOnce {
    type Output = Result<(), SandboxError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.polled.fetch_add(1, Ordering::SeqCst) == 0 {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Ready(self.outcome.take().expect("polled after completion"))
    }
}

fn yield_once(outcome: Result<(), SandboxError>) -> (Arc<AtomicUsize>, YieldOnce) {
    let polled = Arc::new(AtomicUsize::new(0));
    (
        Arc::clone(&polled),
        YieldOnce {
            polled,
            outcome: Some(outcome),
        },
    )
}

#[tokio::test]
async fn t1_completed_ok_settles_without_retaining() {
    let (runs, retain) = retention();
    let (_, effect) = yield_once(Ok(()));
    let claimed = ClaimedEffect::new(EffectClaim::durable("reconciler", retain), effect);

    assert!(claimed.await.is_ok());
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "I3: completion never retains"
    );
}

#[tokio::test]
async fn t2_completed_err_settles_without_retaining() {
    let (runs, retain) = retention();
    let (_, effect) = yield_once(Err(SandboxError::backend(
        "t2_completed_err_settles_without_retaining",
        "the effect failed",
    )));
    let claimed = ClaimedEffect::new(EffectClaim::durable("reconciler", retain), effect);

    assert!(claimed.await.is_err());
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "I3: a failed effect is equally no longer in flight and must not retain"
    );
}

#[test]
fn t3_dropped_before_the_first_poll_retains_once() {
    let (runs, retain) = retention();
    let (polled, effect) = yield_once(Ok(()));
    let claimed = ClaimedEffect::new(EffectClaim::durable("reconciler", retain), effect);

    drop(claimed);

    assert_eq!(polled.load(Ordering::SeqCst), 0, "the effect never started");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "I2: an unpolled drop still retains, because the caller cannot prove nothing ran"
    );
}

#[test]
fn t4_dropped_mid_poll_retains_once() {
    let (runs, retain) = retention();
    let (polled, effect) = yield_once(Ok(()));
    let mut in_flight = Box::pin(
        ClaimedEffect::new(EffectClaim::durable("reconciler", retain), effect).into_future(),
    );

    let mut context = Context::from_waker(Waker::noop());
    assert!(in_flight.as_mut().poll(&mut context).is_pending());
    assert_eq!(polled.load(Ordering::SeqCst), 1, "the effect is in flight");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "an in-flight effect has not settled yet"
    );

    drop(in_flight);

    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "I2: the cancellation window is exactly what retention covers"
    );
}

#[test]
fn t5_abandon_settles_without_retaining() {
    let (runs, retain) = retention();
    let (polled, effect) = yield_once(Ok(()));
    let claimed = ClaimedEffect::new(EffectClaim::durable("reconciler", retain), effect);

    claimed.abandon();

    assert_eq!(polled.load(Ordering::SeqCst), 0);
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "I4: abandoning is the caller stating that no effect was started"
    );
}

#[test]
fn t6_the_into_future_handoff_retains_once_across_two_drop_owners() {
    let (runs, retain) = retention();
    let (polled, effect) = yield_once(Ok(()));
    let in_flight =
        ClaimedEffect::new(EffectClaim::durable("reconciler", retain), effect).into_future();

    // The `ClaimedEffect` husk has already been dropped by `into_future`, so
    // this is the second `Drop` to see the same claim.
    drop(in_flight);

    assert_eq!(polled.load(Ordering::SeqCst), 0);
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "I5: the handoff moves the obligation, it neither discharges nor duplicates it"
    );
}

#[test]
fn i1_the_record_exists_while_the_effect_has_provably_not_started() {
    let (runs, retain) = retention();
    let (polled, effect) = yield_once(Ok(()));
    let claimed = ClaimedEffect::new(EffectClaim::durable("realization_scan", retain), effect);

    assert_eq!(
        claimed.reconciler(),
        Some("realization_scan"),
        "the claim is already minted"
    );
    assert_eq!(
        polled.load(Ordering::SeqCst),
        0,
        "I1: construction cannot poll, so no effect can precede its claim"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    claimed.abandon();
}

#[test]
fn a_self_recording_claim_names_its_reconciler_and_owes_no_retention() {
    let claim = EffectClaim::self_recording("existing_realization_decision");
    assert_eq!(claim.reconciler(), Some("existing_realization_decision"));

    let (polled, effect) = yield_once(Ok(()));
    let claimed: ClaimedEffect<'_, ()> = ClaimedEffect::new(claim, effect);
    assert_eq!(claimed.reconciler(), Some("existing_realization_decision"));
    drop(claimed);
    assert_eq!(
        polled.load(Ordering::SeqCst),
        0,
        "the resource records itself, so dropping runs nothing"
    );
}

#[test]
fn an_in_process_claim_names_no_reconciler_because_nothing_outside_can_read_it() {
    let (runs, retain) = retention();
    let claim = EffectClaim::in_process(retain);
    assert_eq!(claim.reconciler(), None);

    let (_, effect) = yield_once(Ok(()));
    drop(ClaimedEffect::new(claim, effect));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[test]
#[should_panic(expected = "must name the scan")]
fn a_stamp_nobody_reads_is_not_a_record() {
    let _ = EffectClaim::self_recording("   ");
}

/// The same exhaustion the Kani harness proves, run as a test so the kernel is
/// covered even where the model checker is not installed.
#[test]
fn the_claim_state_machine_settles_everything_and_retains_only_an_unsettled_drop() {
    let transitions = [
        ClaimTransition::Completed { ok: true },
        ClaimTransition::Completed { ok: false },
        ClaimTransition::Abandoned,
        ClaimTransition::Dropped,
    ];
    for start in [ClaimDisposition::Unsettled, ClaimDisposition::Settled] {
        for transition in transitions {
            let (next, retained) = advance_claim(start, transition);
            assert_eq!(next, ClaimDisposition::Settled);
            assert_eq!(
                retained,
                start == ClaimDisposition::Unsettled && transition == ClaimTransition::Dropped
            );
            assert!(!advance_claim(next, transition).1, "settling is absorbing");
        }
    }
}
