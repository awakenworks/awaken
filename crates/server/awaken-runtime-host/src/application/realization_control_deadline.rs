//! Authority-derived request deadline decoration for Session realization Control.

use std::sync::Arc;

/// Independent Control requests are bounded per Worker. The same scheduler is
/// shared by every due Session in one sweep; no per-call semaphore is created.
pub(crate) const MAX_CONCURRENT_SESSION_REALIZATION_REQUESTS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlPermitEvent {
    Acquire,
    Returned,
    DeadlineElapsed,
    ScopeEnded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlPermitReleasePlan {
    RetainOwned,
    ReleaseOwned,
    StutterReleased,
    Reject,
}

/// The production lifecycle for one actual Tokio-owned permit. This is not a
/// second capacity counter: the Semaphore remains the sole global capacity
/// authority. The plan decides whether this wrapper must retain or release its
/// one opaque permit after each repository-owned lifecycle event.
const fn control_permit_release_plan(
    has_owned_permit: bool,
    event: ControlPermitEvent,
) -> ControlPermitReleasePlan {
    match (has_owned_permit, event) {
        (true, ControlPermitEvent::Acquire) => ControlPermitReleasePlan::RetainOwned,
        (
            true,
            ControlPermitEvent::Returned
            | ControlPermitEvent::DeadlineElapsed
            | ControlPermitEvent::ScopeEnded,
        ) => ControlPermitReleasePlan::ReleaseOwned,
        // Explicit settlement consumes `self`, whose destructor must remain a
        // harmless ScopeEnded stutter instead of attempting a second release.
        (false, ControlPermitEvent::ScopeEnded) => ControlPermitReleasePlan::StutterReleased,
        (
            false,
            ControlPermitEvent::Acquire
            | ControlPermitEvent::Returned
            | ControlPermitEvent::DeadlineElapsed,
        ) => ControlPermitReleasePlan::Reject,
    }
}

#[derive(Clone)]
struct SessionRealizationControlScheduler {
    permits: Arc<tokio::sync::Semaphore>,
}

impl SessionRealizationControlScheduler {
    fn new() -> Self {
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_SESSION_REALIZATION_REQUESTS,
            )),
        }
    }

    async fn acquire(&self) -> Result<ControlRequestPermit, &'static str> {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "Session realization Control scheduler closed")?;
        Ok(ControlRequestPermit::acquired(permit))
    }
}

pub(super) struct ControlRequestPermit {
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl ControlRequestPermit {
    fn acquired(permit: tokio::sync::OwnedSemaphorePermit) -> Self {
        let mut acquired = Self {
            permit: Some(permit),
        };
        acquired.apply(ControlPermitEvent::Acquire);
        acquired
    }

    fn apply(&mut self, event: ControlPermitEvent) {
        match control_permit_release_plan(self.permit.is_some(), event) {
            ControlPermitReleasePlan::RetainOwned | ControlPermitReleasePlan::StutterReleased => {}
            ControlPermitReleasePlan::ReleaseOwned => {
                // The typed release plan, not a debug-only assertion, drives
                // the physical release in every production build.
                drop(
                    self.permit
                        .take()
                        .expect("release plan must still own one Tokio permit"),
                );
            }
            ControlPermitReleasePlan::Reject => {
                panic!("Control permit event must follow its owned-token lifecycle")
            }
        }
    }

    fn settle(mut self, event: ControlPermitEvent) {
        assert!(matches!(
            event,
            ControlPermitEvent::Returned | ControlPermitEvent::DeadlineElapsed
        ));
        self.apply(event);
    }
}

impl Drop for ControlRequestPermit {
    fn drop(&mut self) {
        // Return/deadline settlement reaches Released first; cancellation,
        // panic unwind, and ordinary scope exit reach this edge while Acquired.
        // Both paths consume the same production ownership plan.
        self.apply(ControlPermitEvent::ScopeEnded);
    }
}

#[derive(Clone)]
pub(super) struct DeadlineSessionRealizationControl<'a> {
    inner: &'a dyn awaken_session_contract::SessionRealizationControl,
    request_timeout: std::time::Duration,
    scheduler: SessionRealizationControlScheduler,
}

impl<'a> DeadlineSessionRealizationControl<'a> {
    pub(super) fn new(
        inner: &'a dyn awaken_session_contract::SessionRealizationControl,
        request_timeout: std::time::Duration,
    ) -> Self {
        Self {
            inner,
            request_timeout,
            scheduler: SessionRealizationControlScheduler::new(),
        }
    }

    /// Re-enter the same bounded scheduler after a Control response or
    /// deadline. The request permit has already been returned, so queued peer
    /// calls can proceed before slow exact-owner cleanup. Holding this permit
    /// then keeps response application and cleanup inside the same global cap.
    pub(super) async fn acquire_completion_permit(
        &self,
    ) -> Result<ControlRequestPermit, awaken_session_contract::SessionRealizationControlFailure>
    {
        self.scheduler.acquire().await.map_err(|message| {
            awaken_session_contract::SessionRealizationControlFailure::Unavailable(message.into())
        })
    }

    async fn call<T>(
        &self,
        operation: &'static str,
        future: impl std::future::Future<
            Output = Result<T, awaken_session_contract::SessionRealizationControlFailure>,
        >,
    ) -> Result<T, awaken_session_contract::SessionRealizationControlFailure> {
        let permit = self.acquire_completion_permit().await?;
        match tokio::time::timeout(self.request_timeout, future).await {
            Ok(result) => {
                permit.settle(ControlPermitEvent::Returned);
                result
            }
            Err(_) => {
                permit.settle(ControlPermitEvent::DeadlineElapsed);
                Err(
                    awaken_session_contract::SessionRealizationControlFailure::Unavailable(
                        format!(
                            "Session realization Control `{operation}` exceeded its authority-derived request deadline"
                        ),
                    ),
                )
            }
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn session_realization_control_permit_ownership_release_is_total_and_exact() {
    let has_owned_permit: bool = kani::any();
    let event = match kani::any::<u8>() % 4 {
        0 => ControlPermitEvent::Acquire,
        1 => ControlPermitEvent::Returned,
        2 => ControlPermitEvent::DeadlineElapsed,
        _ => ControlPermitEvent::ScopeEnded,
    };
    let plan = control_permit_release_plan(has_owned_permit, event);

    assert_eq!(
        plan == ControlPermitReleasePlan::ReleaseOwned,
        has_owned_permit && !matches!(event, ControlPermitEvent::Acquire)
    );
    assert_eq!(
        plan == ControlPermitReleasePlan::RetainOwned,
        has_owned_permit && event == ControlPermitEvent::Acquire
    );
    assert_eq!(
        plan == ControlPermitReleasePlan::StutterReleased,
        !has_owned_permit && event == ControlPermitEvent::ScopeEnded
    );
    assert_eq!(
        plan == ControlPermitReleasePlan::Reject,
        !has_owned_permit && event != ControlPermitEvent::ScopeEnded
    );
}

/// Deadline decoration only bounds transport waiting. The wrapped Control
/// remains the sole owner of leases, phases, cleanup commands, and receipts.
#[async_trait::async_trait]
impl awaken_session_contract::SessionRealizationControl for DeadlineSessionRealizationControl<'_> {
    async fn begin_session_realization(
        &self,
        command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call("begin", self.inner.begin_session_realization(command))
            .await
    }

    async fn renew_session_realization(
        &self,
        command: awaken_session_contract::RenewSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationLeaseReceipt,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call("renew", self.inner.renew_session_realization(command))
            .await
    }

    async fn activate_session_realization(
        &self,
        command: awaken_session_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call("activate", self.inner.activate_session_realization(command))
            .await
    }

    async fn acknowledge_session_realization(
        &self,
        command: awaken_session_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "acknowledge",
            self.inner.acknowledge_session_realization(command),
        )
        .await
    }

    async fn fail_session_realization(
        &self,
        command: awaken_session_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.call("fail", self.inner.fail_session_realization(command))
            .await
    }
}

#[cfg(test)]
mod composition_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_permit_ownership_decision_releases_every_terminal_row_once() {
        // Cause/effect table: C1 the sole Option does or does not contain the
        // actual Tokio permit; C2 the event is acquire, return, deadline, or
        // scope-end. Effects: E1 acquired+Acquire retains it; E2 every acquired
        // terminal row orders one release; E3 destructor-after-settlement
        // stutters; E4 every torn ordering fails closed.
        //
        // | Rule | Owned permit | event | Effect |
        // |---|---|---|---|
        // | P1 | yes | Acquire | E1 retain |
        // | P2 | yes | return/deadline/scope | E2 release |
        // | P3 | no | scope | E3 stutter |
        // | P4 | no | acquire/return/deadline | E4 reject |
        assert_eq!(
            control_permit_release_plan(true, ControlPermitEvent::Acquire),
            ControlPermitReleasePlan::RetainOwned,
            "P1/E1"
        );
        for event in [
            ControlPermitEvent::Returned,
            ControlPermitEvent::DeadlineElapsed,
            ControlPermitEvent::ScopeEnded,
        ] {
            assert_eq!(
                control_permit_release_plan(true, event),
                ControlPermitReleasePlan::ReleaseOwned,
                "P2/E2"
            );
        }
        assert_eq!(
            control_permit_release_plan(false, ControlPermitEvent::ScopeEnded),
            ControlPermitReleasePlan::StutterReleased,
            "P3/E3"
        );
        for event in [
            ControlPermitEvent::Acquire,
            ControlPermitEvent::Returned,
            ControlPermitEvent::DeadlineElapsed,
        ] {
            assert_eq!(
                control_permit_release_plan(false, event),
                ControlPermitReleasePlan::Reject,
                "P4/E4"
            );
        }
    }

    #[tokio::test]
    async fn owned_capacity_returns_after_return_deadline_cancel_panic_and_drop() {
        // Runtime cause/effect table: C1 one production scheduler acquires an
        // actual Tokio OwnedSemaphorePermit; C2 it leaves by Control return,
        // deadline, task cancellation, panic unwind, or ordinary drop. Effect
        // E1 every row restores exactly the configured capacity. Acquisition
        // and wake/drop semantics are Tokio TCB; this test checks that each Rust
        // path consumes the production typed release plan and no shadow count.
        //
        // | Rule | Exit | Production edge | Effect |
        // |---|---|---|---|
        // | R1 | return | settle(Returned) | E1 |
        // | R2 | timeout | settle(DeadlineElapsed) | E1 |
        // | R3 | cancel | Drop(ScopeEnded) | E1 |
        // | R4 | panic | unwind Drop(ScopeEnded) | E1 |
        // | R5 | ordinary scope | Drop(ScopeEnded) | E1 |
        let scheduler = SessionRealizationControlScheduler::new();
        let capacity = MAX_CONCURRENT_SESSION_REALIZATION_REQUESTS;

        tokio::time::timeout(std::time::Duration::from_secs(30), scheduler.acquire())
            .await
            .expect("timed out waiting for scheduler.acquire()")
            .expect("R1 acquire")
            .settle(ControlPermitEvent::Returned);
        assert_eq!(scheduler.permits.available_permits(), capacity, "R1/E1");

        tokio::time::timeout(std::time::Duration::from_secs(30), scheduler.acquire())
            .await
            .expect("timed out waiting for scheduler.acquire()")
            .expect("R2 acquire")
            .settle(ControlPermitEvent::DeadlineElapsed);
        assert_eq!(scheduler.permits.available_permits(), capacity, "R2/E1");

        let cancelled =
            tokio::time::timeout(std::time::Duration::from_secs(30), scheduler.acquire())
                .await
                .expect("timed out waiting for scheduler.acquire()")
                .expect("R3 acquire");
        let cancelled_task = tokio::spawn(async move {
            let _permit = cancelled;
            std::future::pending::<()>().await;
        });
        cancelled_task.abort();
        assert!(cancelled_task.await.unwrap_err().is_cancelled(), "R3");
        assert_eq!(scheduler.permits.available_permits(), capacity, "R3/E1");

        let panicking =
            tokio::time::timeout(std::time::Duration::from_secs(30), scheduler.acquire())
                .await
                .expect("timed out waiting for scheduler.acquire()")
                .expect("R4 acquire");
        let panicking_task = tokio::spawn(async move {
            let _permit = panicking;
            panic!("R4 injected permit-owner panic");
        });
        assert!(panicking_task.await.unwrap_err().is_panic(), "R4");
        assert_eq!(scheduler.permits.available_permits(), capacity, "R4/E1");

        let dropped = tokio::time::timeout(std::time::Duration::from_secs(30), scheduler.acquire())
            .await
            .expect("timed out waiting for scheduler.acquire()")
            .expect("R5 acquire");
        drop(dropped);
        assert_eq!(scheduler.permits.available_permits(), capacity, "R5/E1");
    }
}
