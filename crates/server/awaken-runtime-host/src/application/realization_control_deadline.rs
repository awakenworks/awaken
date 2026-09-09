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
struct ControlCapacityState {
    active: usize,
}

/// Pure transition used by the production permit adapter. Tokio owns wakeup
/// and mutex/semaphore mechanics; this kernel owns the fail-closed capacity and
/// release decision for every repository-visible request outcome.
const fn control_capacity_transition(
    state: ControlCapacityState,
    capacity: usize,
    event: ControlPermitEvent,
) -> Option<ControlCapacityState> {
    match event {
        ControlPermitEvent::Acquire if state.active < capacity => Some(ControlCapacityState {
            active: state.active + 1,
        }),
        ControlPermitEvent::Returned
        | ControlPermitEvent::DeadlineElapsed
        | ControlPermitEvent::ScopeEnded
            if state.active > 0 =>
        {
            Some(ControlCapacityState {
                active: state.active - 1,
            })
        }
        ControlPermitEvent::Acquire
        | ControlPermitEvent::Returned
        | ControlPermitEvent::DeadlineElapsed
        | ControlPermitEvent::ScopeEnded => None,
    }
}

#[derive(Clone)]
struct SessionRealizationControlScheduler {
    permits: Arc<tokio::sync::Semaphore>,
}

impl SessionRealizationControlScheduler {
    fn new() -> Self {
        let initial = ControlCapacityState { active: 0 };
        debug_assert_eq!(
            control_capacity_transition(
                initial,
                MAX_CONCURRENT_SESSION_REALIZATION_REQUESTS,
                ControlPermitEvent::Acquire,
            ),
            Some(ControlCapacityState { active: 1 })
        );
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
        Ok(ControlRequestPermit {
            permit: Some(permit),
        })
    }
}

pub(super) struct ControlRequestPermit {
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl ControlRequestPermit {
    fn settle(mut self, event: ControlPermitEvent) {
        debug_assert!(matches!(
            event,
            ControlPermitEvent::Returned | ControlPermitEvent::DeadlineElapsed
        ));
        debug_assert_eq!(
            control_capacity_transition(
                ControlCapacityState { active: 1 },
                MAX_CONCURRENT_SESSION_REALIZATION_REQUESTS,
                event,
            ),
            Some(ControlCapacityState { active: 0 })
        );
        // Dropping the sole owned permit is the actual production release edge.
        self.permit.take();
    }
}

impl Drop for ControlRequestPermit {
    fn drop(&mut self) {
        // Completion or cancellation follows the same kernel edge. The Tokio
        // owned permit is then dropped by Rust field destruction and returns
        // one capacity slot.
        debug_assert_eq!(
            control_capacity_transition(
                ControlCapacityState {
                    active: usize::from(self.permit.is_some()),
                },
                MAX_CONCURRENT_SESSION_REALIZATION_REQUESTS,
                ControlPermitEvent::ScopeEnded,
            ),
            self.permit
                .is_some()
                .then_some(ControlCapacityState { active: 0 })
        );
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
fn session_realization_control_permit_is_bounded_and_every_settlement_releases() {
    let capacity: usize = kani::any();
    let active: usize = kani::any();
    let event = match kani::any::<u8>() % 4 {
        0 => ControlPermitEvent::Acquire,
        1 => ControlPermitEvent::Returned,
        2 => ControlPermitEvent::DeadlineElapsed,
        _ => ControlPermitEvent::ScopeEnded,
    };
    let state = ControlCapacityState { active };
    let next = control_capacity_transition(state, capacity, event);

    match event {
        ControlPermitEvent::Acquire => {
            assert_eq!(next.is_some(), active < capacity);
            if let Some(next) = next {
                assert_eq!(next.active, active + 1);
                assert!(next.active <= capacity);
            }
        }
        ControlPermitEvent::Returned
        | ControlPermitEvent::DeadlineElapsed
        | ControlPermitEvent::ScopeEnded => {
            assert_eq!(next.is_some(), active > 0);
            if let Some(next) = next {
                assert_eq!(next.active, active - 1);
                assert!(next.active < active);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_capacity_decision_is_bounded_and_releases_every_terminal_row() {
        // Cause/effect table: C1 active requests are below/at capacity; C2 the
        // event acquires, returns, reaches its deadline, or is cancelled.
        // Effects: E1 acquire advances only below the bound; E2 every terminal
        // event releases exactly one occupied slot; E3 invalid underflow and
        // over-capacity transitions fail closed.
        //
        // | Rule | active/capacity | event | Effect |
        // |---|---|---|---|
        // | C1 | 0/1 | acquire | E1 -> 1 |
        // | C2 | 1/1 | acquire | E3 |
        // | C3 | 1/1 | return/deadline/cancel | E2 -> 0 |
        // | C4 | 0/1 | return/deadline/cancel | E3 |
        let empty = ControlCapacityState { active: 0 };
        let occupied = ControlCapacityState { active: 1 };
        assert_eq!(
            control_capacity_transition(empty, 1, ControlPermitEvent::Acquire),
            Some(occupied),
            "C1/E1"
        );
        assert_eq!(
            control_capacity_transition(occupied, 1, ControlPermitEvent::Acquire),
            None,
            "C2/E3"
        );
        for event in [
            ControlPermitEvent::Returned,
            ControlPermitEvent::DeadlineElapsed,
            ControlPermitEvent::ScopeEnded,
        ] {
            assert_eq!(
                control_capacity_transition(occupied, 1, event),
                Some(empty),
                "C3/E2 {event:?}"
            );
            assert_eq!(
                control_capacity_transition(empty, 1, event),
                None,
                "C4/E3 {event:?}"
            );
        }
    }
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

    async fn claim_next_terminal_cleanup(
        &self,
        target: awaken_session_contract::SessionRealizationTarget,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupAssignment>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "claim_terminal_cleanup",
            self.inner.claim_next_terminal_cleanup(target),
        )
        .await
    }

    async fn terminal_cleanup_work(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupWork>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "poll_terminal_cleanup",
            self.inner.terminal_cleanup_work(session_id, lease),
        )
        .await
    }

    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "authorize_terminal_cleanup_effect",
            self.inner.authorize_terminal_cleanup_effect(effect),
        )
        .await
    }

    async fn authorize_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "authorize_terminal_cleanup_disposal",
            self.inner.authorize_terminal_cleanup_disposal(effect),
        )
        .await
    }

    async fn authorize_checkpoint_release_artifact_effect(
        &self,
        session_id: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
    ) -> Result<String, awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "authorize_checkpoint_release_artifact_effect",
            self.inner
                .authorize_checkpoint_release_artifact_effect(session_id, operation),
        )
        .await
    }

    async fn authorize_terminal_memory_intent(
        &self,
        intent: &awaken_session_contract::SessionTerminalMemoryIntent,
    ) -> Result<
        awaken_session_contract::SessionTerminalMemoryTarget,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "authorize_terminal_memory_intent",
            self.inner.authorize_terminal_memory_intent(intent),
        )
        .await
    }

    async fn terminal_repository_publication_command(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "poll_terminal_repository_publication",
            self.inner
                .terminal_repository_publication_command(session_id, lease),
        )
        .await
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "record_terminal_repository_publication",
            self.inner
                .record_terminal_repository_publication_receipt(session_id, lease, receipt),
        )
        .await
    }

    async fn record_terminal_repository_publication_rejection(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
        rejection: awaken_session_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "record_terminal_repository_publication_rejection",
            self.inner
                .record_terminal_repository_publication_rejection(session_id, lease, rejection),
        )
        .await
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        lease: &awaken_session_contract::SessionRealizationLease,
        preparation: awaken_session_contract::SessionCleanupPreparation,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "record_terminal_cleanup_preparation",
            self.inner
                .record_terminal_cleanup_preparation(lease, preparation),
        )
        .await
    }

    async fn record_terminal_cleanup_disposal(
        &self,
        lease: &awaken_session_contract::SessionRealizationLease,
        receipt: awaken_session_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "record_terminal_cleanup_disposal",
            self.inner.record_terminal_cleanup_disposal(lease, receipt),
        )
        .await
    }
}
