//! Exact revision-bound credential refresh factory port.

use std::sync::Arc;

use awaken_credential::CredentialRefresher;
use awaken_credential_contract::CredentialSourceId;
use awaken_runtime_contract::CredentialRefreshAccess;

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
