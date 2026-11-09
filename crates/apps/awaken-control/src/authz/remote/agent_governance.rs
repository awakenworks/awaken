// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Current direct Account and exact issuer come from native IAM;
// @formal reason: all current questions use the separate single-attempt transport.
use super::{RemoteManagementAuthz, build_current_gate, unix_seconds_at};
use awaken_iam_client::IamDecisionError;
use awaken_iam_contract::{
    AccountId, AuthorizationEvaluation, RegisteredResourceAuthorizationEvaluation,
    RegisteredResourceAuthorizationRequest, ResourceAuthorizationRequest,
};
use awaken_iam_host::{AuthReject, IamGate};

impl RemoteManagementAuthz {
    fn current_governance_gate(&self) -> Result<IamGate, String> {
        // Refresh the shared carrier generation first. A rotation clears this
        // lazily constructed current-decision transport in the same lock.
        let _ = self.gate_for_authorization()?;
        let mut state = self
            .gate
            .write()
            .map_err(|_| "Cloud IAM gate lock is poisoned".to_owned())?;
        if let Some(gate) = state.current_gate.as_ref() {
            return Ok(gate.clone());
        }
        let gate = build_current_gate(&self.config, state.user_carrier_token.as_ref())?;
        state.current_gate = Some(gate.clone());
        Ok(gate)
    }

    pub(crate) fn current_agent_governance_identity(
        &self,
        token: &str,
    ) -> Result<(String, AccountId), AuthReject> {
        let gate = self
            .current_governance_gate()
            .map_err(|_| AuthReject::Unavailable)?;
        let account =
            gate.authenticate_current_account(token, unix_seconds_at(self.clock.as_ref()))?;
        // authenticate_current_account has already verified this exact configured
        // issuer/audience and live native Account. No request claim or URL fallback.
        let issuer = gate
            .expected_issuer()
            .ok_or(AuthReject::Unavailable)?
            .to_owned();
        Ok((issuer, account))
    }
    pub(crate) fn agent_governance_observation(
        &self,
        request: ResourceAuthorizationRequest,
    ) -> Result<AuthorizationEvaluation, IamDecisionError> {
        self.current_governance_gate()
            .map_err(|_| IamDecisionError::Unavailable)?
            .authorize_resource_evaluation(request)
    }
    pub(crate) fn agent_governance_authorization(
        &self,
        request: RegisteredResourceAuthorizationRequest,
    ) -> Result<RegisteredResourceAuthorizationEvaluation, IamDecisionError> {
        self.current_governance_gate()
            .map_err(|_| IamDecisionError::Unavailable)?
            .authorize_registered_resource(request)
    }
}
