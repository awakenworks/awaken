// @formal entrypoint session_execution.one_physical_attempt_per_thread
//! The one process-local participant barrier of a physical attempt.
//!
//! Tokio owns task counting/drain; Dispatch owns durable occupancy. This
//! barrier closes entry and retains unknown evidence without asserting a
//! remote provider stopped.

use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

const fn entry_is_open(closed: bool) -> bool {
    !closed
}
const fn return_is_confirmed(active: usize, uncertain: bool) -> bool {
    active == 0 && !uncertain
}

#[derive(Default)]
struct Barrier {
    tasks: TaskTracker,
    entry: Mutex<()>,
    uncertainty: CancellationToken,
    model_result_unknown: CancellationToken,
}

/// Attempt-local evidence shared with inline children. A parent return cannot
/// cover an active child. TaskTracker permits insertion after close by itself,
/// so first-poll enrollment and close use one mutex here. Its tracker, tokens
/// and closure are private; no caller can reopen or bypass the entry barrier.
#[derive(Clone, Default)]
pub struct AttemptQuiescence {
    barrier: Arc<Barrier>,
}

struct PendingReturn<'a> {
    quiescence: &'a AttemptQuiescence,
    returned: bool,
    response_only: bool,
    _participant: TaskTrackerToken,
}

impl Drop for PendingReturn<'_> {
    fn drop(&mut self) {
        // Report the veto BEFORE the tracker token drops and wakes the parent.
        if !self.returned {
            self.quiescence.mark_model_uncertain(if self.response_only {
                crate::model_operation::ModelEffectScope::ResponseOnly
            } else {
                crate::model_operation::ModelEffectScope::Opaque
            });
        }
    }
}

impl AttemptQuiescence {
    /// Enroll on first poll. A closed context is a programming error: refuse it
    /// before polling the supplied effect. Normal return disarms only this
    /// observer; Future drop/unwind retains uncertainty in the same barrier.
    pub async fn observe_return<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.observe_participant(future, false).await
    }

    /// Data-only response observation uses the same entry/count/drain owner.
    /// Dropping its consumer vetoes continuation, but is not an assertion that
    /// provider computation stopped or a veto from an unknown tool effect.
    pub async fn observe_model_return<T>(
        &self,
        scope: crate::model_operation::ModelEffectScope,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        self.observe_participant(
            future,
            scope == crate::model_operation::ModelEffectScope::ResponseOnly,
        )
        .await
    }

    async fn observe_participant<T>(
        &self,
        future: impl std::future::Future<Output = T>,
        response_only: bool,
    ) -> T {
        let participant = {
            let _entry = self.barrier.entry.lock().expect("attempt barrier poisoned");
            entry_is_open(self.barrier.tasks.is_closed()).then(|| self.barrier.tasks.token())
        };
        // Reject outside the mutex; stale entry cannot poison an old barrier.
        let participant = participant.expect("cannot enter a closed physical attempt");
        let mut pending = PendingReturn {
            quiescence: self,
            returned: false,
            response_only,
            _participant: participant,
        };
        let result = future.await;
        pending.returned = true;
        drop(pending);
        result
    }

    pub fn mark_uncertain(&self) {
        self.barrier.uncertainty.cancel();
    }

    pub fn mark_model_uncertain(&self, scope: crate::model_operation::ModelEffectScope) {
        match scope {
            crate::model_operation::ModelEffectScope::Opaque => self.mark_uncertain(),
            crate::model_operation::ModelEffectScope::ResponseOnly => {
                self.barrier.model_result_unknown.cancel()
            }
        }
    }

    /// Continuation differs from return: nested execution inside an observed
    /// child is legal until closure/uncertainty. This grants no Dispatch claim.
    #[must_use]
    pub fn permits_execution(&self) -> bool {
        entry_is_open(self.barrier.tasks.is_closed())
            && !self.barrier.uncertainty.is_cancelled()
            && !self.barrier.model_result_unknown.is_cancelled()
    }

    /// Read-only observation; Dispatch must close entry before its exact ACK.
    #[must_use]
    pub fn can_acknowledge_return(&self) -> bool {
        return_is_confirmed(
            self.barrier.tasks.len(),
            self.barrier.uncertainty.is_cancelled(),
        )
    }

    /// A signed Worker may durably report only this local fact. It proves no
    /// remote stop and grants no retirement; Coordinator closes contained
    /// process domains while the original Dispatch slot remains occupied.
    pub fn local_entry_closed(&self) -> bool {
        self.barrier.tasks.is_closed() && self.barrier.tasks.is_empty()
    }

    /// Close entry and drain owned participants. Uncertainty vetoes ACK even if
    /// other participants remain pending. No timeout can substitute for return;
    /// TaskTracker and CancellationToken own wake-up correctness.
    pub async fn close_and_wait(&self) -> bool {
        {
            let _entry = self.barrier.entry.lock().expect("attempt barrier poisoned");
            self.barrier.tasks.close();
        }
        tokio::select! {
            biased;
            _ = self.barrier.uncertainty.cancelled() => false,
            _ = self.barrier.tasks.wait() => self.can_acknowledge_return(),
        }
    }
}

#[cfg(test)]
mod quiescence_tests {
    use super::*;
    use crate::runtime_context::RuntimeRunContext;
    use futures_util::FutureExt;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn discarded_response_closes_continuation_without_erasing_effect_uncertainty() {
        use crate::model_operation::ModelEffectScope;
        // Decision table: C1 data-only/opaque adapter; C2 independent tool
        // uncertainty absent/present. Drop the entered consumer in every row.
        // R1=data-only+!C2 => unknown result, zero new execution, local closed
        // return may ACK. R2=opaque|C2 => retained physical veto. No row claims
        // provider termination or permits replay of the discarded invocation.
        for scope in [ModelEffectScope::ResponseOnly, ModelEffectScope::Opaque] {
            for tool_unknown in [false, true] {
                let barrier = AttemptQuiescence::default();
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(1),
                        barrier.observe_model_return(scope, std::future::pending::<()>())
                    )
                    .await
                    .is_err()
                );
                if tool_unknown {
                    barrier.mark_uncertain();
                }
                assert!(!barrier.permits_execution());
                let ack = barrier.close_and_wait().await;
                assert!(barrier.local_entry_closed());
                assert_eq!(
                    ack,
                    scope == ModelEffectScope::ResponseOnly && !tool_unknown
                );
            }
        }
    }

    #[tokio::test]
    async fn only_an_entered_unreturned_future_vetoes_acknowledgement() {
        // C1 never polled / C2 returns Ok or Err / C3 polled then dropped.
        // R1=C1=>no invented activity; R2=C2=>local return observed;
        // R3=C3=>monotonic veto. Error return is not a remote terminal proof;
        // definitive error classification remains an adapter obligation.
        let latch = AttemptQuiescence::default();
        drop(latch.observe_return(std::future::pending::<()>()));
        assert!(latch.can_acknowledge_return());
        for response in [Ok(()), Err(())] {
            assert_eq!(latch.observe_return(async { response }).await, response);
            assert!(latch.can_acknowledge_return());
        }
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(1),
                latch.observe_return(std::future::pending::<()>())
            )
            .await
            .is_err()
        );
        assert!(!latch.can_acknowledge_return());
        latch.observe_return(async {}).await;
        assert!(
            !latch.can_acknowledge_return(),
            "later return cannot erase uncertainty"
        );
    }

    #[test]
    fn uncertainty_is_shared_with_inline_children_and_never_cleared_by_return() {
        // Q1 fresh attempt -> return can ACK. Q2 an inline child's discarded
        // effect -> every context clone vetoes ACK. Q3 duplicate reports ->
        // stutter. Q4 separately constructed attempt -> independent evidence.
        // This verifies handle propagation, not arbitrary scheduler interleavings.
        let root = RuntimeRunContext::new();
        assert!(root.physical_quiescence.can_acknowledge_return(), "Q1");
        let child = root.for_child_run();
        child.physical_quiescence.mark_uncertain();
        child.physical_quiescence.mark_uncertain();
        drop(child);
        assert!(!root.physical_quiescence.can_acknowledge_return(), "Q2/Q3");
        assert!(
            RuntimeRunContext::new()
                .physical_quiescence
                .can_acknowledge_return(),
            "Q4"
        );
    }

    #[tokio::test]
    async fn close_drains_active_children_and_rejects_delayed_entry_without_effect() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Causes: C1 child has entered; C2 root returns; C3 closure competes
        // with a not-yet-polled future; C4 the entered child eventually returns.
        // Effects: E1 active work vetoes ACK without vetoing nested execution;
        // E2 close denies all new entry; E3 old child drains then ACK succeeds;
        // E4 the delayed effect is never polled and cannot poison the barrier.
        // Rules R1=C1 => !ACK + execution; R2=C1+C2 => wait + !entry;
        // R3=R2+C4 => ACK; R4=C3 after close => zero effect + unchanged ACK.
        let latch = AttemptQuiescence::default();
        let child = latch.clone();
        let entered = Arc::new(Notify::new());
        let released = Arc::new(Notify::new());
        let start = entered.clone();
        let finish = released.clone();
        let task = tokio::spawn(async move {
            child
                .observe_return(async {
                    start.notify_one();
                    tokio::time::timeout(std::time::Duration::from_secs(5), finish.notified())
                        .await
                        .expect("child released");
                })
                .await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .expect("child entered");
        assert!(!latch.can_acknowledge_return(), "R1 active child");
        assert!(latch.permits_execution(), "R1 inline child may continue");
        let effected = AtomicBool::new(false);
        let delayed = latch.observe_return(async {
            effected.store(true, Ordering::SeqCst);
        });
        let close = latch.close_and_wait();
        tokio::pin!(close);
        assert!(close.as_mut().now_or_never().is_none(), "R2 drain");
        assert!(!latch.permits_execution(), "R2 entry barrier closed");
        released.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("child joined")
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), close)
                .await
                .expect("owned drain returned"),
            "R3 real child return"
        );
        assert!(
            std::panic::AssertUnwindSafe(delayed)
                .catch_unwind()
                .await
                .is_err(),
            "R4 reject delayed entry"
        );
        assert!(!effected.load(Ordering::SeqCst), "R4 zero effect");
        assert!(latch.can_acknowledge_return(), "R4 no mutex poisoning");
        assert!(
            latch.close_and_wait().await,
            "R3 repeated close is idempotent"
        );
    }

    #[tokio::test]
    async fn dropped_child_wakes_close_but_never_fabricates_ack() {
        // R5=C1 enrolled child + C2 waiting closure + C3 future drop/panic =>
        // closure wakes with false, active count is retired, uncertainty stays.
        // R6 a later normal return in a separately owned attempt => true and
        // cannot erase R5. Notification registration must precede the read.
        for unwind in [false, true] {
            let latch = AttemptQuiescence::default();
            let mut pending = Box::pin(latch.observe_return(std::future::pending::<()>()));
            assert!(pending.as_mut().now_or_never().is_none());
            let close = latch.close_and_wait();
            tokio::pin!(close);
            assert!(close.as_mut().now_or_never().is_none());
            if unwind {
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _owned = pending;
                        panic!("participant unwind");
                    }))
                    .is_err()
                );
            } else {
                drop(pending);
            }
            assert!(
                !tokio::time::timeout(std::time::Duration::from_secs(1), close)
                    .await
                    .unwrap(),
                "R5 {unwind}"
            );
            assert!(!latch.can_acknowledge_return());
            let independent = AttemptQuiescence::default();
            independent.observe_return(async {}).await;
            assert!(independent.close_and_wait().await, "R6");
        }
    }

    #[tokio::test]
    async fn closure_decision_table_requires_every_participant_to_return() {
        // C1 0..3 participants; C2 every combination of normal return/drop;
        // C3 independent uncertainty; C4 duplicate close. For all 30 rows,
        // R7=all returned + no uncertainty => true; R8=any drop/unknown => false.
        // A normal Err return is a local return, not a provider stop proof:
        // adapters remain responsible for reporting opaque remote uncertainty.
        for count in 0..=3 {
            for drops in 0..(1 << count) {
                for uncertain in [false, true] {
                    let latch = AttemptQuiescence::default();
                    let mut participants = Vec::new();
                    for _ in 0..count {
                        let (release, result) = tokio::sync::oneshot::channel::<()>();
                        let mut participant = Box::pin(latch.observe_return(async {
                            result.await.expect("owned result delivered");
                            Err::<(), ()>(())
                        }));
                        assert!(participant.as_mut().now_or_never().is_none());
                        participants.push((release, participant));
                    }
                    for (index, (release, participant)) in participants.into_iter().enumerate() {
                        if drops & (1 << index) != 0 {
                            drop(participant);
                        } else {
                            release.send(()).unwrap();
                            assert_eq!(participant.await, Err(()));
                        }
                    }
                    if uncertain {
                        latch.mark_uncertain();
                    }
                    let expected = drops == 0 && !uncertain;
                    assert_eq!(
                        latch.close_and_wait().await,
                        expected,
                        "R7/R8 count={count} drops={drops} unknown={uncertain}"
                    );
                    assert_eq!(latch.close_and_wait().await, expected, "R4 repeated close");
                }
            }
        }
    }
}
