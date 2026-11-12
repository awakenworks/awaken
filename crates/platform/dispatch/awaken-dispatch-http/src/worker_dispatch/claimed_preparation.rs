// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: The authenticated Session-resume edge borrows its original exact-claim guard;
// @formal reason: it requires an empty physical slot and never treats lease expiry as stop evidence.
//! Borrow the original Dispatch exclusion for one authenticated preparation.

use awaken_dispatch_contract::{CommitEpochGuard, RunDispatch};
use awaken_session_execution_contract::{
    ClaimedSessionCredentialPreparationContext, SessionRealizationControlFailure,
};
use awaken_sync::InvariantLock;

struct ClaimedPreparation<'a> {
    // The store guard is Send, deliberately not Sync. This mutex exposes only
    // short read observations and is never held across an await or effect.
    guard: InvariantLock<&'a mut CommitEpochGuard>,
    request: RunDispatch,
    claim: awaken_worker_contract::RunClaim,
    work: awaken_session_application_contract::SessionWorkAuthorityReceipt,
}

impl<'a> ClaimedPreparation<'a> {
    fn new(
        guard: &'a mut CommitEpochGuard,
        claim: &awaken_worker_contract::RunClaim,
        work: &awaken_session_application_contract::SessionWorkAuthorityReceipt,
    ) -> Self {
        let request = guard.request().clone();
        Self {
            guard: InvariantLock::new(guard),
            request,
            claim: claim.clone(),
            work: work.clone(),
        }
    }
}

impl ClaimedSessionCredentialPreparationContext for ClaimedPreparation<'_> {
    fn claim(&self) -> &awaken_worker_contract::RunClaim {
        &self.claim
    }
    fn session_id(&self) -> &str {
        &self.request.session_thread_id().0
    }
    fn thread_id(&self) -> &awaken_agent_contract::ThreadId {
        self.request.thread_id()
    }
    fn activation(&self) -> &awaken_runtime_contract::RunActivation {
        &self.request.activation
    }
    fn execution_scope(&self) -> Option<&awaken_agent_contract::ExecutionScopeRef> {
        self.request.execution_scope.as_ref()
    }
    fn session_work_authority(
        &self,
    ) -> &awaken_session_application_contract::SessionWorkAuthorityReceipt {
        &self.work
    }
    fn verify_at(&self, now: u64) -> Result<(), SessionRealizationControlFailure> {
        let guard = self.guard.lock().map_err(|_| {
            SessionRealizationControlFailure::Unavailable(
                "claimed preparation guard is poisoned".into(),
            )
        })?;
        if guard.request() != &self.request
            || self.request.run_id() != &self.claim.run_id
            || self.request.activation.thread_id != *self.request.thread_id()
            || self.work.session_id().as_str() != self.session_id()
            || self.work.worker_owner() != self.claim.owner
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "claimed preparation does not match the original Run and Session Work".into(),
            ));
        }
        if !guard.is_live_at(now) || guard.cancellation_requested() {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        if !guard.permits_settlement() {
            return Err(SessionRealizationControlFailure::NotReady);
        }
        Ok(())
    }
}

/// Authenticated transport preparation borrows the original Dispatch exclusion;
/// physical realization stays entirely behind the existing Session control.
pub(super) async fn begin(
    control: &dyn awaken_session_execution_contract::SessionRealizationControl,
    guard: &mut CommitEpochGuard,
    claim: &awaken_worker_contract::RunClaim,
    work: &awaken_session_application_contract::SessionWorkAuthorityReceipt,
    clock: &dyn awaken_time::WallClock,
    command: awaken_session_execution_contract::BeginSessionRealization,
) -> Result<
    awaken_session_execution_contract::SessionRealizationDirective,
    SessionRealizationControlFailure,
> {
    use awaken_time::WallClockExt as _;
    let context = ClaimedPreparation::new(guard, claim, work);
    context.verify_at(clock.now_ms())?;
    control
        .begin_claimed_session_realization(&context, command)
        .await
}

/// Release the original backend exclusion before reporting assignment success.
pub(super) async fn release(guard: CommitEpochGuard) -> Result<(), super::RealizationHttpError> {
    guard.release().await.map_err(|message| {
        super::RealizationHttpError::Control(SessionRealizationControlFailure::Unavailable(message))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claimed_preparation_requires_exact_live_uncancelled_empty_physical_slot() {
        use awaken_dispatch_contract::PhysicalAttemptSlot;
        let request = RunDispatch::new(awaken_dispatch_testkit::worker_http::activation("prepare"));
        let claim = awaken_worker_contract::RunClaim {
            run_id: request.run_id().clone(),
            owner: "worker/boot".into(),
            epoch: 1,
        };
        // This unit table starts after the HTTP owner's independent canonical
        // Work observation. Decoding this assertion alone grants no authority.
        let work = serde_json::from_value(serde_json::json!({
            "session_id": request.session_thread_id().0,
            "worker_owner": claim.owner,
            "authority": {"kind": "not_required"},
        }))
        .unwrap();
        for (slot, cancelled, now, allowed) in [
            (PhysicalAttemptSlot::Empty, false, 9, true),
            (PhysicalAttemptSlot::Empty, false, 10, false),
            (PhysicalAttemptSlot::Empty, true, 9, false),
            (PhysicalAttemptSlot::ExactClaim, false, 9, false),
            (PhysicalAttemptSlot::OtherClaim, false, 9, false),
            (PhysicalAttemptSlot::Corrupt, false, 9, false),
        ] {
            let mut guard = CommitEpochGuard::new((), request.clone(), 10, cancelled, slot);
            let context = ClaimedPreparation::new(&mut guard, &claim, &work);
            assert_eq!(
                context.verify_at(now).is_ok(),
                allowed,
                "slot={slot:?} cancelled={cancelled} now={now}"
            );
            assert_eq!(
                context.activation(),
                &request.activation,
                "frozen input is retained"
            );
        }
        let mut foreign = claim.clone();
        foreign.run_id = awaken_agent_contract::RunId("foreign".into());
        let mut guard = CommitEpochGuard::new((), request, 10, false, PhysicalAttemptSlot::Empty);
        assert!(
            ClaimedPreparation::new(&mut guard, &foreign, &work)
                .verify_at(9)
                .is_err()
        );
    }

    #[test]
    fn poisoned_claimed_preparation_guard_refuses_authority() {
        use awaken_dispatch_contract::PhysicalAttemptSlot;
        let request =
            RunDispatch::new(awaken_dispatch_testkit::worker_http::activation("poisoned"));
        let claim = awaken_worker_contract::RunClaim {
            run_id: request.run_id().clone(),
            owner: "worker/boot".into(),
            epoch: 1,
        };
        let work = serde_json::from_value(serde_json::json!({
            "session_id": request.session_thread_id().0,
            "worker_owner": claim.owner,
            "authority": {"kind": "not_required"},
        }))
        .unwrap();
        let mut guard = CommitEpochGuard::new((), request, 10, false, PhysicalAttemptSlot::Empty);
        let context = ClaimedPreparation::new(&mut guard, &claim, &work);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = context
                .guard
                .lock()
                .unwrap_or_else(|_| panic!("guard poisoned before the test fault"));
            panic!("fixture poisons the retained guard observation");
        }));
        assert!(poisoned.is_err());
        assert!(matches!(
            context.verify_at(9),
            Err(SessionRealizationControlFailure::Unavailable(_))
        ));
    }
}
