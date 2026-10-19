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

#[test]
fn mapped_effect_defers_mapper_and_keeps_original_claim_through_every_boundary() {
    // Graph: construct -> mapper preflight -> original effect -> result wrap
    // -> completion. Each async stage has a real Pending observation. Causes:
    // C1 cancel at construction/preflight/effect/wrap; C2 reject preflight;
    // C3 original completes Ok/Err; C4 explicitly abandon before polling;
    // C5 result wrapping completes Err after the original completed Ok.
    // Effects: E1 original retention exactly once on an unsettled drop;
    // E2 completion/abandon never retains; E3 preflight rejection never polls
    // the original; E4 the original reconciler and projected result survive.
    // | Row | terminal boundary | polls (preflight, original, wrap) | retention |
    // | M1  | unpolled drop     | 0,0,0                            | 1 |
    // | M2  | preflight Pending | 1,0,0                            | 1 |
    // | M3  | original Pending  | 2,1,0                            | 1 |
    // | M4  | wrap Pending      | 2,2,1                            | 1 |
    // | M5  | success           | 2,2,2                            | 0 |
    // | M6  | original Err      | 2,2,0                            | 0 |
    // | M7  | preflight Err     | 0,0,0                            | 0 |
    // | M8  | abandon           | 0,0,0                            | 0 |
    // | M9  | wrap Err          | 2,2,2                            | 0 |
    // Constraints: preflight Err excludes original/wrap; original Err excludes
    // wrap; abandon cannot follow IntoFuture because that consumes the value.
    // Durable mode exercises both retention and reconciler; mapping has no
    // mode-dependent branch, and the existing three-mode tests remain intact.
    for row in [
        "unpolled",
        "preflight",
        "original",
        "wrap",
        "success",
        "original_error",
        "preflight_error",
        "abandon",
        "wrap_error",
    ] {
        let (runs, retain) = retention();
        let original_error = SandboxError::conflict("original effect failure");
        let metadata_error = SandboxError::invalid("metadata rejected");
        let expected_metadata_error = metadata_error.clone();
        let wrap_error = SandboxError::conflict("result wrapper rejected");
        let (original_polls, original) = yield_once(if row == "original_error" {
            Err(original_error.clone())
        } else {
            Ok(())
        });
        let (preflight_polls, preflight) = yield_once(Ok(()));
        let (wrap_polls, wrap) = yield_once(if row == "wrap_error" {
            Err(wrap_error.clone())
        } else {
            Ok(())
        });
        let mapper_calls = Arc::new(AtomicUsize::new(0));
        let calls = mapper_calls.clone();
        let mapped = ClaimedEffect::new(EffectClaim::durable("original_scan", retain), original)
            .map_future(move |original| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if row == "preflight_error" {
                        return Err(metadata_error);
                    }
                    preflight.await?;
                    original.await?;
                    wrap.await?;
                    Ok(7_u8)
                }
            });
        assert_eq!(mapped.reconciler(), Some("original_scan"), "{row} E4");
        assert_eq!(
            mapper_calls.load(Ordering::SeqCst),
            0,
            "{row} construction cannot invoke mapper"
        );
        assert_eq!(
            original_polls.load(Ordering::SeqCst),
            0,
            "{row} construction cannot start physical I/O"
        );
        assert_eq!(
            runs.load(Ordering::SeqCst),
            0,
            "{row} handoff cannot discharge or duplicate claim"
        );
        if matches!(row, "unpolled" | "abandon") {
            if row == "abandon" {
                mapped.abandon();
            } else {
                drop(mapped);
            }
            assert_eq!(
                mapper_calls.load(Ordering::SeqCst),
                0,
                "M1/M8 mapper never runs"
            );
            assert_eq!(
                original_polls.load(Ordering::SeqCst),
                0,
                "M1/M8 original never runs"
            );
            assert_eq!(
                preflight_polls.load(Ordering::SeqCst),
                0,
                "M1/M8 preflight never runs"
            );
            assert_eq!(
                wrap_polls.load(Ordering::SeqCst),
                0,
                "M1/M8 wrap never runs"
            );
            assert_eq!(
                runs.load(Ordering::SeqCst),
                usize::from(row == "unpolled"),
                "M1/M8"
            );
            continue;
        }
        let mut in_flight = Box::pin(mapped.into_future());
        let mut cx = Context::from_waker(Waker::noop());
        let pending_polls = match row {
            "preflight" => Some(1),
            "original" => Some(2),
            "wrap" => Some(3),
            _ => None,
        };
        if let Some(count) = pending_polls {
            for _ in 0..count {
                assert!(
                    in_flight.as_mut().poll(&mut cx).is_pending(),
                    "{row} real Pending"
                );
            }
        } else {
            let mut result = None;
            for _ in 0..4 {
                if let Poll::Ready(outcome) = in_flight.as_mut().poll(&mut cx) {
                    result = Some(outcome);
                    break;
                }
            }
            let expected = match row {
                "original_error" => Err(original_error),
                "preflight_error" => Err(expected_metadata_error),
                "wrap_error" => Err(wrap_error),
                "success" => Ok(7_u8),
                _ => unreachable!(),
            };
            assert_eq!(
                result,
                Some(expected),
                "{row} E3/E4 original error or projected result"
            );
        }
        let expected_polls = match row {
            "preflight" => (1, 0, 0),
            "original" => (2, 1, 0),
            "wrap" => (2, 2, 1),
            "success" | "wrap_error" => (2, 2, 2),
            "original_error" => (2, 2, 0),
            "preflight_error" => (0, 0, 0),
            _ => unreachable!(),
        };
        assert_eq!(
            (
                preflight_polls.load(Ordering::SeqCst),
                original_polls.load(Ordering::SeqCst),
                wrap_polls.load(Ordering::SeqCst)
            ),
            expected_polls,
            "{row}"
        );
        assert_eq!(
            mapper_calls.load(Ordering::SeqCst),
            1,
            "{row} mapper is one shot"
        );
        assert_eq!(
            runs.load(Ordering::SeqCst),
            0,
            "{row} original claim still belongs to outer owner"
        );
        drop(in_flight);
        assert_eq!(
            runs.load(Ordering::SeqCst),
            usize::from(pending_polls.is_some()),
            "{row} E1/E2"
        );
    }
}

#[test]
fn a_mapper_panic_on_first_poll_retains_the_original_claim_only_when_dropped() {
    // P1 construct -> mapper=0, physical=0, retention=0. P2 first poll invokes
    // the mapper and it panics before touching original -> mapper=1, physical=0,
    // retention=0 while the in-flight owner remains alive. P3 dropping that
    // owner -> original retention=1. No second claim or panic-cleanup protocol.
    let (runs, retain) = retention();
    let (polled, original) = yield_once(Ok(()));
    let mapper_calls = Arc::new(AtomicUsize::new(0));
    let calls = mapper_calls.clone();
    let mapped = ClaimedEffect::new(EffectClaim::durable("original_scan", retain), original)
        .map_future(
            move |_original| -> std::future::Ready<Result<u8, SandboxError>> {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("injected mapper construction panic");
            },
        );
    assert_eq!(mapped.reconciler(), Some("original_scan"), "P1");
    assert_eq!(mapper_calls.load(Ordering::SeqCst), 0, "P1");
    assert_eq!(polled.load(Ordering::SeqCst), 0, "P1");
    let mut in_flight = Box::pin(mapped.into_future());
    let mut cx = Context::from_waker(Waker::noop());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = in_flight.as_mut().poll(&mut cx);
    }));
    assert!(panic.is_err(), "P2");
    assert_eq!(mapper_calls.load(Ordering::SeqCst), 1, "P2");
    assert_eq!(
        polled.load(Ordering::SeqCst),
        0,
        "P2 original future never polled"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "P2 original in-flight owner remains live"
    );
    drop(in_flight);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "P3 same original retention");
}

#[test]
fn repeated_future_projection_keeps_one_original_claim_and_reconciler() {
    // Inductive handoff table: one/three identity projections followed by a
    // result-type projection, crossed with unpolled drop, Pending drop,
    // abandon, Ok, and Err. Every settled husk must be inert; the final owner
    // keeps the same reconciler. Drop rows retain once; explicit terminal rows
    // never retain. The original future is polled only by the final owner.
    for depth in [1, 3] {
        for outcome in ["drop", "pending", "abandon", "ok", "error"] {
            let (runs, retain) = retention();
            let original_error = SandboxError::conflict("original error");
            let (polled, original) = yield_once(if outcome == "error" {
                Err(original_error.clone())
            } else {
                Ok(())
            });
            let mut projected =
                ClaimedEffect::new(EffectClaim::durable("original_scan", retain), original);
            for _ in 0..depth {
                projected = projected.map_future(std::convert::identity);
                assert_eq!(
                    projected.reconciler(),
                    Some("original_scan"),
                    "{depth}/{outcome}"
                );
                assert_eq!(
                    polled.load(Ordering::SeqCst),
                    0,
                    "{depth}/{outcome} no early effect"
                );
                assert_eq!(
                    runs.load(Ordering::SeqCst),
                    0,
                    "{depth}/{outcome} inert husk"
                );
            }
            let projected = projected.map_future(|original| async move {
                original.await?;
                Ok(19_u8)
            });
            assert_eq!(
                projected.reconciler(),
                Some("original_scan"),
                "{depth}/{outcome}"
            );
            if outcome == "abandon" {
                projected.abandon();
                assert_eq!(polled.load(Ordering::SeqCst), 0);
                assert_eq!(runs.load(Ordering::SeqCst), 0);
                continue;
            }
            if outcome == "drop" {
                drop(projected);
                assert_eq!(polled.load(Ordering::SeqCst), 0);
                assert_eq!(runs.load(Ordering::SeqCst), 1);
                continue;
            }
            let mut in_flight = Box::pin(projected.into_future());
            let mut cx = Context::from_waker(Waker::noop());
            assert!(
                in_flight.as_mut().poll(&mut cx).is_pending(),
                "{depth}/{outcome} real Pending"
            );
            if outcome == "pending" {
                drop(in_flight);
                assert_eq!(polled.load(Ordering::SeqCst), 1);
                assert_eq!(runs.load(Ordering::SeqCst), 1);
            } else {
                assert_eq!(
                    in_flight.as_mut().poll(&mut cx),
                    Poll::Ready(if outcome == "error" {
                        Err(original_error)
                    } else {
                        Ok(19_u8)
                    })
                );
                drop(in_flight);
                assert_eq!(polled.load(Ordering::SeqCst), 2);
                assert_eq!(runs.load(Ordering::SeqCst), 0);
            }
        }
    }
}
