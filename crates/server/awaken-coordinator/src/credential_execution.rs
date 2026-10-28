// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Execution admission reads the original Session root, current dispatch claim and exact physical attempt; rechecks reject state changes during observation.
//! Read-only credential admission over the original Session and Dispatch owners.
//! No grant database, application lease or login state is introduced here.
use awaken_credential_contract::{
    CredentialExecutionAuthority, CredentialExecutionBinding, CredentialMaterialError,
};
use awaken_dispatch_contract::{
    DispatchClaimOwnership, DispatchControlReads, RunClaim, SessionRunAdmissionObservation,
    SessionRunAdmissionReads,
};
use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_session_repository_contract::SessionRepository;
use std::sync::Arc;

pub struct SessionCredentialExecutionAuthority {
    sessions: Arc<dyn SessionRepository>,
    dispatch: Arc<AnyCoordinatorDispatch>,
}
impl SessionCredentialExecutionAuthority {
    pub fn new(
        sessions: Arc<dyn SessionRepository>,
        dispatch: Arc<AnyCoordinatorDispatch>,
    ) -> Self {
        Self { sessions, dispatch }
    }
}
#[async_trait::async_trait]
impl CredentialExecutionAuthority for SessionCredentialExecutionAuthority {
    async fn resolve_current(
        &self,
        workspace: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<CredentialExecutionBinding, CredentialMaterialError> {
        use CredentialMaterialError::{BindingMismatch, Unavailable};
        let run = awaken_agent_contract::agent::run::Id(run_id.to_string());
        let SessionRunAdmissionObservation::Live(request) = self
            .dispatch
            .observe_session_run_admission(&run)
            .await
            .map_err(|_| Unavailable)?
        else {
            return Err(BindingMismatch);
        };
        let origin = request
            .session_thread_id
            .as_ref()
            .unwrap_or(&request.activation.thread_id)
            .0
            .as_str();
        if !session_id.is_empty() && session_id != origin {
            return Err(BindingMismatch);
        }
        let id = awaken_session_types::SessionId::from(origin);
        if self.sessions.owner(&id).await.map_err(|_| Unavailable)? != workspace {
            return Err(BindingMismatch);
        }
        let session = self.sessions.get(&id).await.map_err(|_| Unavailable)?;
        let physical = self
            .dispatch
            .load_physical_attempt(&run)
            .await
            .map_err(|_| Unavailable)?
            .ok_or(BindingMismatch)?;
        let binding = CredentialExecutionBinding {
            session_id: origin.into(),
            agent_id: request.activation.snapshot.root_agent_id.0.clone(),
            environment_id: session.environment_id().into(),
            run_id: run_id.into(),
            claim_owner: physical.claim.owner.clone(),
            claim_epoch: physical
                .claim
                .epoch
                .try_into()
                .map_err(|_| BindingMismatch)?,
            execution_ref: physical
                .binding
                .as_ref()
                .ok_or(BindingMismatch)?
                .execution_ref()
                .into(),
        };
        self.assert_current(workspace, &binding).await?;
        Ok(binding)
    }
    async fn assert_current(
        &self,
        workspace: &str,
        b: &CredentialExecutionBinding,
    ) -> Result<(), CredentialMaterialError> {
        use CredentialMaterialError::{BindingMismatch, Unavailable};
        b.validate()?;
        let session_id = awaken_session_types::SessionId::from(b.session_id.as_str());
        if self
            .sessions
            .owner(&session_id)
            .await
            .map_err(|_| Unavailable)?
            != workspace
        {
            return Err(BindingMismatch);
        }
        let session = self
            .sessions
            .get(&session_id)
            .await
            .map_err(|_| Unavailable)?;
        if session.is_terminal()
            || session.is_hidden()
            || session.agent_id().is_none()
            || session.environment_id() != b.environment_id
        {
            return Err(BindingMismatch);
        }
        let claim = RunClaim {
            run_id: awaken_agent_contract::agent::run::Id(b.run_id.clone()),
            owner: b.claim_owner.clone(),
            epoch: b.claim_epoch.get(),
        };
        if !self
            .dispatch
            .claim_is_current(&claim)
            .await
            .map_err(|_| Unavailable)?
        {
            return Err(BindingMismatch);
        }
        let SessionRunAdmissionObservation::Live(request) = self
            .dispatch
            .observe_session_run_admission(&claim.run_id)
            .await
            .map_err(|_| Unavailable)?
        else {
            return Err(BindingMismatch);
        };
        let origin = request
            .session_thread_id
            .as_ref()
            .unwrap_or(&request.activation.thread_id);
        if origin.0 != b.session_id
            || request.activation.snapshot.root_agent_id.0 != b.agent_id
            || request.execution_scope.as_ref().map(|s| s.0.as_str()) != Some(workspace)
        {
            return Err(BindingMismatch);
        }
        let physical = self
            .dispatch
            .load_physical_attempt(&claim.run_id)
            .await
            .map_err(|_| Unavailable)?
            .ok_or(BindingMismatch)?;
        physical.validate_admission().map_err(|_| BindingMismatch)?;
        if physical.claim != claim
            || physical.binding.as_ref().map(|p| p.execution_ref())
                != Some(b.execution_ref.as_str())
        {
            return Err(BindingMismatch);
        }
        // A Session transition or claim takeover during the independent reads
        // cannot turn a stale observation into a material-use grant.
        let current = self
            .sessions
            .get(&session_id)
            .await
            .map_err(|_| Unavailable)?;
        if current.revision != session.revision
            || current.is_terminal()
            || current.is_hidden()
            || !self
                .dispatch
                .claim_is_current(&claim)
                .await
                .map_err(|_| Unavailable)?
        {
            return Err(BindingMismatch);
        }
        Ok(())
    }
}
