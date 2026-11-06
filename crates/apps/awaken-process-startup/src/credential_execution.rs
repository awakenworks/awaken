// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Assembly forwards to the original Session/Dispatch authority and refuses an unbound port; this classification is not a proof claim.
//! One-time assembly wiring, not a credential, Session or lease store.
use awaken_credential_contract::{
    CredentialExecutionAuthority, CredentialExecutionBinding, CredentialMaterialError,
};
use std::sync::{Arc, OnceLock};
#[derive(Default)]
pub(crate) struct ExecutionAuthoritySlot(OnceLock<Arc<dyn CredentialExecutionAuthority>>);
impl ExecutionAuthoritySlot {
    pub(crate) fn install(
        &self,
        authority: Arc<dyn CredentialExecutionAuthority>,
    ) -> Result<(), String> {
        self.0
            .set(authority)
            .map_err(|_| "credential execution authority already installed".to_string())
    }
}
#[async_trait::async_trait]
impl CredentialExecutionAuthority for ExecutionAuthoritySlot {
    async fn resolve_current(
        &self,
        workspace: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<CredentialExecutionBinding, CredentialMaterialError> {
        self.0
            .get()
            .ok_or(CredentialMaterialError::Unavailable)?
            .resolve_current(workspace, session_id, run_id)
            .await
    }
    async fn assert_current(
        &self,
        workspace: &str,
        execution: &CredentialExecutionBinding,
    ) -> Result<(), CredentialMaterialError> {
        self.0
            .get()
            .ok_or(CredentialMaterialError::Unavailable)?
            .assert_current(workspace, execution)
            .await
    }
}
