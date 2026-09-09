//! Controlled-poll qualification of the existing two-stage scheduler adapter.
use super::*;
use std::cell::Cell;
use std::rc::Rc;
use std::task::Poll;

struct EnteredRequest(Rc<Cell<bool>>);
impl Drop for EnteredRequest {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

#[tokio::test(start_paused = true)]
async fn scheduler_capacity_observes_real_poll_drop_and_two_stage_cancellation() {
    // Causes: all eight permits occupied; request queued versus acquired;
    // inner outcome success/error/deadline/cancellation; completion queued
    // versus held. Effects: queue residence does not start the request timer;
    // every actual owned guard releases exactly once; queued cancellation
    // releases none; response completion uses the identical semaphore.
    // R1-R7 separately cover four cancellation cuts and three returning outcomes.
    // Completion-cancellation rows use a successful response prefix; this is
    // seven decision rules, not a Cartesian product of every outcome and cut.
    // Existing Host M1-M3 covers the subsequent *actual* Session mutex/cleanup
    // pipeline. This test invokes adapter operations, not a replacement driver.
    // Every snapshot is taken after a controlled poll/drop on one Tokio thread:
    // there is no observer lock, concurrent counter sampling, or trusted scalar
    // capacity reducer. The independent inner guard measures actual polling and
    // destruction; the reserved Vec contains actual production owned permits.
    let mut rows = Vec::new();
    for outcome in [
        "ok",
        "error",
        "deadline",
        "cancel-request",
        "cancel-queued-request",
        "cancel-queued-completion",
        "cancel-held-completion",
    ] {
        let inner = crate::host::RemoteTerminalCleanupControl::default();
        let control =
            DeadlineSessionRealizationControl::new(&inner, std::time::Duration::from_millis(10));
        let scheduler = format!("{:p}", Arc::as_ptr(&control.scheduler.permits));
        let mut reserved = Vec::new();
        for _ in 0..MAX_CONCURRENT_SESSION_REALIZATION_REQUESTS {
            reserved.push(control.acquire_completion_permit().await.unwrap());
        }
        let active = Rc::new(Cell::new(false));
        let observed = active.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut request = Box::pin(control.call("qualification", async move {
            observed.set(true);
            let _entered = EnteredRequest(observed);
            receive.await.expect("the test owns the response")
        }));
        let mut trace = Vec::new();
        let snapshot = |trace: &mut Vec<serde_json::Value>,
                        event: &str,
                        request: &str,
                        completion: &str,
                        reserved: usize| {
            let available = control.scheduler.permits.available_permits();
            let held_completion = usize::from(completion == "held");
            assert_eq!(
                available + reserved + usize::from(active.get()) + held_completion,
                MAX_CONCURRENT_SESSION_REALIZATION_REQUESTS,
                "S1 exact actual permit conservation: {outcome}/{event}"
            );
            trace.push(
                serde_json::json!({"sequence":trace.len(), "scheduler":scheduler,
                "event":event, "request":request, "completion":completion,
                "available":available, "reserved":reserved, "active":active.get()}),
            );
        };
        snapshot(&mut trace, "initial", "none", "none", reserved.len());
        assert!(futures_util::poll!(&mut request).is_pending());
        assert!(!active.get(), "S2 queued request has not polled transport");
        snapshot(
            &mut trace,
            "queue-request",
            "queued",
            "none",
            reserved.len(),
        );
        tokio::time::advance(std::time::Duration::from_millis(20)).await;
        assert!(
            futures_util::poll!(&mut request).is_pending(),
            "S2 queue has no request deadline"
        );
        snapshot(&mut trace, "wait-queued", "queued", "none", reserved.len());
        if outcome == "cancel-queued-request" {
            drop(request);
            snapshot(
                &mut trace,
                "cancel-queued-request",
                "done",
                "none",
                reserved.len(),
            );
        } else {
            drop(reserved.pop());
            // A woken acquire may reserve capacity before it is polled. Observe
            // only after the selected request has consumed that wakeup.
            assert!(
                futures_util::poll!(&mut request).is_pending(),
                "S2 timer starts only after admission"
            );
            assert!(
                active.get(),
                "S3 real transport owns the acquired request permit"
            );
            snapshot(&mut trace, "admit-request", "held", "none", reserved.len());
            if outcome == "cancel-request" {
                drop(request);
                snapshot(&mut trace, "cancel-request", "done", "none", reserved.len());
            } else {
                if outcome == "deadline" {
                    tokio::time::advance(std::time::Duration::from_millis(11)).await;
                } else {
                    send.send(if outcome == "error" {
                        Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
                    } else {
                        Ok(())
                    })
                    .unwrap();
                }
                let Poll::Ready(result) = futures_util::poll!(&mut request) else {
                    panic!("S4 response/deadline must settle the actual call");
                };
                match outcome {
                    "deadline" => assert!(matches!(
                        result,
                        Err(
                            awaken_session_contract::SessionRealizationControlFailure::Unavailable(
                                _
                            )
                        )
                    )),
                    "error" => assert!(matches!(
                        result,
                        Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
                    )),
                    _ => assert!(result.is_ok()),
                }
                drop(request);
                snapshot(
                    &mut trace,
                    if outcome == "deadline" {
                        "deadline"
                    } else {
                        "return"
                    },
                    "done",
                    "none",
                    reserved.len(),
                );
                assert!(!active.get(), "S4 settled transport future was destroyed");
                // Refill the released slot through that same scheduler. A
                // queued completion must not acquire a second unbounded lane.
                reserved.push(control.acquire_completion_permit().await.unwrap());
                let mut completion = Box::pin(control.acquire_completion_permit());
                assert!(
                    futures_util::poll!(&mut completion).is_pending(),
                    "S5 completion shares capacity"
                );
                snapshot(
                    &mut trace,
                    "queue-completion",
                    "done",
                    "queued",
                    reserved.len(),
                );
                if outcome == "cancel-queued-completion" {
                    drop(completion);
                    snapshot(
                        &mut trace,
                        "cancel-queued-completion",
                        "done",
                        "done",
                        reserved.len(),
                    );
                } else {
                    drop(reserved.pop());
                    let Poll::Ready(Ok(permit)) = futures_util::poll!(&mut completion) else {
                        panic!("S6 released slot admits exactly one completion");
                    };
                    drop(completion);
                    snapshot(
                        &mut trace,
                        "admit-completion",
                        "done",
                        "held",
                        reserved.len(),
                    );
                    drop(permit);
                    snapshot(
                        &mut trace,
                        if outcome == "cancel-held-completion" {
                            "cancel-held-completion"
                        } else {
                            "finish"
                        },
                        "done",
                        "done",
                        reserved.len(),
                    );
                }
            }
        }
        drop(reserved);
        snapshot(&mut trace, "final", "done", "done", 0);
        rows.push(serde_json::json!({"rule":outcome, "observation":{"kind":"adapter", "capacity":MAX_CONCURRENT_SESSION_REALIZATION_REQUESTS, "trace":trace}}));
    }
    awaken_reliability_testkit::write_observation_trace(
        "realization-capacity-cancellation",
        "realization-capacity",
        serde_json::json!(rows),
    );
}
