// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Existing dispatch admission and settlement effects over the one Session application.
// @formal reason: The contract constructs the exact committed boundary; this adapter owns no receipt,
// @formal reason: queue, claim or retry state. Native tables preserve the original callback boundaries.
//! Session activity admission and boundary observation without a Session executor.

use std::sync::Arc;

use awaken_agent_contract::agent::{
    run::{Id as RunId, RunState},
    thread::Id as ThreadId,
};
use awaken_dispatch_contract::{DispatchSettlementError, DispatchSettlementObserver, RunDispatch};
use awaken_dispatch_execution::SessionRunAdmissionObserver;
use awaken_session_execution_contract::{
    SessionAgentBoundaryCommand, SessionAgentCoordination, SessionRunActivityAdmission,
    SessionRunActivityAdmissionMode,
};
use awaken_session_types::SessionId;
use awaken_worker_contract::RunClaim;

/// The Coordinator's dispatch observers over its original Session application.
/// The root supplies one immutable application. This adapter takes no executor,
/// model or Sandbox port; the application retains its own collaborators. SDK
/// weak endpoints borrow this same adapter per call.
pub struct SessionDispatchObserver {
    application: Arc<dyn SessionAgentCoordination>,
}

impl SessionDispatchObserver {
    #[must_use]
    pub fn new(application: Arc<dyn SessionAgentCoordination>) -> Self {
        Self { application }
    }

    /// The original reservation-repair guard: only an exact self-affine Run
    /// without an already-admitted activity may ask the Session for a receipt.
    #[must_use]
    pub fn activity_session<'a>(
        dispatch: &'a RunDispatch,
        claim: &RunClaim,
    ) -> Option<&'a ThreadId> {
        let session = dispatch.session_thread_id.as_ref()?;
        (dispatch.run_id() == &claim.run_id
            && dispatch.thread_id() == session
            && dispatch.session_activity_epoch.is_none())
        .then_some(session)
    }

    /// Map a Worker's explicit affinity through the contract's sole boundary
    /// constructor. Authenticated HTTP uses its own guarded affinity selection
    /// with that same neutral constructor.
    pub fn boundary_command(
        dispatch: &RunDispatch,
        claim: &RunClaim,
        state: &RunState,
        cancellation_requested: bool,
    ) -> Result<Option<SessionAgentBoundaryCommand>, DispatchSettlementError> {
        SessionAgentBoundaryCommand::from_claimed_boundary(
            dispatch.session_activity_epoch,
            dispatch.session_thread_id.as_ref(),
            (
                dispatch.thread_id(),
                dispatch.run_id(),
                &dispatch.activation.snapshot.root_agent_id.0,
            ),
            &claim.run_id,
            state,
            cancellation_requested,
        )
        .map_err(|error| DispatchSettlementError(error.message))
    }

    /// The already-refined activity request. The SDK's weak endpoint uses this
    /// after the same reservation guard, retaining the application only for the
    /// active call. Receipt replay and admission policy stay application-owned.
    pub async fn admit_activity(
        &self,
        session: &SessionId,
        agent: &str,
        run: &RunId,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, DispatchSettlementError> {
        self.application
            .admit_session_run_activity(session, agent, run, mode)
            .await
            .map_err(|error| DispatchSettlementError(error.to_string()))
    }

    /// Observe an exact boundary before the existing Dispatch settlement CAS.
    pub async fn settle_boundary(
        &self,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), DispatchSettlementError> {
        self.application
            .settle_session_agent_boundary(command)
            .await
            .map_err(|error| DispatchSettlementError(error.to_string()))
    }

    /// Finalize only after that Dispatch CAS; this issues no queue operation.
    pub async fn finalize_boundary(
        &self,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), DispatchSettlementError> {
        self.application
            .finalize_session_agent_boundary(command)
            .await
            .map_err(|error| DispatchSettlementError(error.to_string()))
    }
}

#[async_trait::async_trait]
impl SessionRunAdmissionObserver for SessionDispatchObserver {
    async fn prepare_session_run_activity(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<Option<awaken_session_types::SessionRevision>, DispatchSettlementError> {
        if dispatch.identity_scope
            != awaken_dispatch_contract::DispatchIdentityScope::PreparedSessionCommand
        {
            return Ok(None);
        }
        let session = Self::activity_session(dispatch, claim).ok_or_else(|| {
            DispatchSettlementError("prepared reservation affinity is invalid".into())
        })?;
        self.application
            .prepare_prepared_session_run_activity(
                &SessionId::from(&session.0),
                &dispatch.activation.snapshot.root_agent_id.0,
                dispatch.run_id(),
                mode,
            )
            .await
            .map_err(|error| DispatchSettlementError(error.to_string()))
    }

    async fn admit_session_run_activity(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        mode: SessionRunActivityAdmissionMode,
        preparation: Option<awaken_session_execution_contract::PreparedSessionRunActivity>,
    ) -> Result<SessionRunActivityAdmission, DispatchSettlementError> {
        let Some(session) = Self::activity_session(dispatch, claim) else {
            return Ok(SessionRunActivityAdmission::Rejected);
        };
        if dispatch.identity_scope
            == awaken_dispatch_contract::DispatchIdentityScope::PreparedSessionCommand
        {
            let preparation = preparation.ok_or_else(|| {
                DispatchSettlementError("prepared reservation has no guarded preparation".into())
            })?;
            return self
                .application
                .admit_prepared_session_run_activity(
                    &SessionId::from(&session.0),
                    dispatch.run_id(),
                    mode,
                    preparation,
                )
                .await
                .map_err(|error| DispatchSettlementError(error.to_string()));
        }
        self.admit_activity(
            &SessionId::from(&session.0),
            &dispatch.activation.snapshot.root_agent_id.0,
            dispatch.run_id(),
            mode,
        )
        .await
    }
}

#[async_trait::async_trait]
impl DispatchSettlementObserver for SessionDispatchObserver {
    fn owns_dispatch_fence(&self) -> bool {
        false
    }

    async fn before_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        state: &RunState,
        cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        let Some(command) = Self::boundary_command(dispatch, claim, state, cancellation_requested)?
        else {
            return Ok(());
        };
        self.settle_boundary(command).await
    }

    async fn after_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        state: &RunState,
        cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        let Some(command) = Self::boundary_command(dispatch, claim, state, cancellation_requested)?
        else {
            return Ok(());
        };
        self.finalize_boundary(command).await
    }
}

/// The same selected application configures a Coordinator's commit-boundary
/// Worker through its existing observer roles. No executor capability is added.
#[async_trait::async_trait]
impl crate::SessionBoundaryEffects for SessionDispatchObserver {
    async fn configure(
        self: Arc<Self>,
        worker: awaken_dispatch_execution::DispatchWorker<
            awaken_dispatch_erasure::AnyDispatchStore,
        >,
        _claimed: &awaken_dispatch_contract::Claimed,
        _commit: Arc<crate::SessionCommit>,
        context: Option<awaken_runtime_contract::RuntimeRunContext>,
    ) -> Result<
        awaken_dispatch_execution::DispatchWorker<awaken_dispatch_erasure::AnyDispatchStore>,
        awaken_dispatch_execution::Error,
    > {
        let worker = worker
            .with_settlement_observer(self.clone())
            .with_session_run_admission_observer(self);
        Ok(match context {
            Some(context) => worker.with_context(context),
            None => worker,
        })
    }
}

#[cfg(test)]
mod tests;
