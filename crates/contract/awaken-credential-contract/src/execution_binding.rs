// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: The coordinate is not a grant; all fields participate in identity and the owning Session/Dispatch port must validate current authority.
//! Secret-free origin of one application credential use. This is a coordinate,
//! never a grant: the Session/Dispatch owner must verify it on every use.
use crate::CredentialMaterialError;
use async_trait::async_trait;
use awaken_lease_contract::fence::Epoch;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialExecutionBinding {
    pub session_id: String,
    pub agent_id: String,
    pub environment_id: String,
    pub run_id: String,
    pub claim_owner: String,
    pub claim_epoch: Epoch,
    pub execution_ref: String,
}
impl CredentialExecutionBinding {
    /// Every origin coordinate participates in equality; no matching subset
    /// can authorize a binding from another Session or physical attempt.
    fn identity(&self) -> (&str, &str, &str, &str, &str, Epoch, &str) {
        (
            &self.session_id,
            &self.agent_id,
            &self.environment_id,
            &self.run_id,
            &self.claim_owner,
            self.claim_epoch,
            &self.execution_ref,
        )
    }
    pub fn validate(&self) -> Result<(), CredentialMaterialError> {
        if [
            &self.session_id,
            &self.agent_id,
            &self.environment_id,
            &self.run_id,
            &self.claim_owner,
            &self.execution_ref,
        ]
        .iter()
        .any(|v| v.trim().is_empty() || v.len() > 512)
        {
            return Err(CredentialMaterialError::BindingMismatch);
        }
        Ok(())
    }
}
impl PartialEq for CredentialExecutionBinding {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

/// Implemented over existing Session roots and live Dispatch claims. Control
/// consumes this narrow role and owns no second Session or execution store.
#[async_trait]
pub trait CredentialExecutionAuthority: Send + Sync {
    /// Resolve the original Run dispatch. An empty Session selector asks the
    /// owner to derive its parent Session; a supplied selector must match it.
    /// Neither selector is execution authority without the live checks.
    async fn resolve_current(
        &self,
        workspace: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<CredentialExecutionBinding, CredentialMaterialError>;
    async fn assert_current(
        &self,
        workspace: &str,
        execution: &CredentialExecutionBinding,
    ) -> Result<(), CredentialMaterialError>;
}
