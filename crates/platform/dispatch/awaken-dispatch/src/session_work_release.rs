// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The independent-store adapter keeps the existing claim guard across the
// @formal reason: original Session Work port and explicitly releases it; its authorization is the shared contract kernel.
//! Conditional Work release where the stores do not borrow the guarded connection.
use awaken_dispatch_contract::dispatch::PhysicalAttempt;
use awaken_dispatch_contract::{
    ConditionalSessionWorkRelease, DispatchCommitAuthority, DispatchError,
    session_work_release_is_authorized,
};
use awaken_session_application_contract::{
    SessionWorkerLeaseAuthority, release_owned_session_work,
};
use awaken_worker_contract::RunClaim;
use std::sync::Arc;

pub struct ClaimGuardedSessionWorkRelease {
    dispatch: Arc<dyn DispatchCommitAuthority>,
    work: Arc<dyn SessionWorkerLeaseAuthority>,
}
impl ClaimGuardedSessionWorkRelease {
    #[must_use]
    pub fn new(
        dispatch: Arc<dyn DispatchCommitAuthority>,
        work: Arc<dyn SessionWorkerLeaseAuthority>,
    ) -> Self {
        Self { dispatch, work }
    }
}
#[async_trait::async_trait]
impl ConditionalSessionWorkRelease for ClaimGuardedSessionWorkRelease {
    async fn release(
        &self,
        claim: &RunClaim,
        observed: &PhysicalAttempt,
        model_only: bool,
    ) -> Result<bool, DispatchError> {
        let Some(guard) = self.dispatch.lock_commit_epoch(claim).await? else {
            return Ok(false);
        };
        let permitted =
            session_work_release_is_authorized(guard.observation(), claim, observed, model_only);
        let result = if permitted {
            release_owned_session_work(
                self.work.as_ref(),
                &awaken_session_types::SessionId::from(
                    guard.request().session_thread_id().0.clone(),
                ),
                &claim.owner,
            )
            .await
        } else {
            Ok(false)
        };
        guard
            .release_after(result)
            .await
            .map_err(DispatchError::unavailable)?
            .map_err(DispatchError::unavailable)
    }
}
