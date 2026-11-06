//! Exact revision-bound credential refresh factory port. Control implements it
//! over its Vault (`VaultRefreshFactory`) and the Worker's credential
//! realization consumes it, so the port is named here and Control links no
//! Worker-side crate.

use std::sync::Arc;

use crate::CredentialSourceId;
use awaken_agent_contract::credential::CredentialRefreshAccess;
use awaken_agent_contract::credential::CredentialRefresher;

/// Factory for one exact credential revision's challenge recovery. Runtime
/// carries only the resulting transport refresher and never receives a
/// Credential repository or Secret Store.
pub trait CredentialRefreshFactory: Send + Sync {
    fn refresher(
        &self,
        credential_id: CredentialSourceId,
        access: CredentialRefreshAccess,
    ) -> Arc<dyn CredentialRefresher>;

    fn bearer_reloader(
        &self,
        credential_id: CredentialSourceId,
        credential_revision: u64,
    ) -> Arc<dyn CredentialRefresher>;
}
