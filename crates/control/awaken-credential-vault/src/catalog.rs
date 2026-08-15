//! Secret-free Managed Vault catalog.
//!
//! The catalog owns the management projection that is not part of a
//! [`CredentialSource`](crate::CredentialSource). Secret material remains owned
//! exclusively by [`SecretStore`](crate::SecretStore), while a credential entry
//! points at the exact source aggregate behind it.

use std::collections::BTreeMap;

use awaken_credential_contract::{CredentialSourceId, TokenEndpointAuth};

use crate::CredentialError;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedVault {
    pub id: String,
    pub workspace_id: String,
    pub display_name: String,
    pub metadata: BTreeMap<String, String>,
    pub archived_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManagedCredentialNetworking {
    Unrestricted,
    Limited { allowed_hosts: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedMcpOauthRefresh {
    pub client_id: String,
    pub token_endpoint: String,
    pub token_endpoint_auth: TokenEndpointAuth,
    pub resource: Option<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManagedCredentialAuth {
    EnvironmentVariable {
        secret_name: String,
        networking: ManagedCredentialNetworking,
    },
    StaticBearer {
        mcp_server_url: String,
    },
    McpOauth {
        mcp_server_url: String,
        expires_at: Option<String>,
        refresh: Option<ManagedMcpOauthRefresh>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedVaultCredential {
    pub id: String,
    pub vault_id: String,
    pub workspace_id: String,
    pub source_id: CredentialSourceId,
    pub auth: ManagedCredentialAuth,
    pub metadata: BTreeMap<String, String>,
    pub display_name: Option<String>,
    pub archived_at: Option<String>,
}

/// One durable authority for the secret-free Managed Vault projection.
/// Implementations must cascade `delete_vault` to its credential entries.
#[async_trait::async_trait]
pub trait ManagedVaultRepo: Send + Sync {
    async fn put_vault(&self, vault: ManagedVault) -> Result<(), CredentialError>;
    async fn get_vault(&self, id: &str) -> Result<Option<ManagedVault>, CredentialError>;
    async fn list_vaults(&self, workspace_id: &str) -> Result<Vec<ManagedVault>, CredentialError>;
    async fn delete_vault(&self, id: &str) -> Result<bool, CredentialError>;

    async fn put_vault_credential(
        &self,
        credential: ManagedVaultCredential,
    ) -> Result<(), CredentialError>;
    async fn get_vault_credential(
        &self,
        id: &str,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError>;
    async fn get_vault_credential_by_source(
        &self,
        source_id: &CredentialSourceId,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError>;
    async fn list_vault_credentials(
        &self,
        vault_id: &str,
    ) -> Result<Vec<ManagedVaultCredential>, CredentialError>;
    async fn delete_vault_credential(&self, id: &str) -> Result<bool, CredentialError>;
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
impl ManagedVaultRepo for crate::repo::InMemoryCredentialRepo {
    async fn put_vault(&self, vault: ManagedVault) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .vaults
            .insert(vault.id.clone(), vault);
        Ok(())
    }

    async fn get_vault(&self, id: &str) -> Result<Option<ManagedVault>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .vaults
            .get(id)
            .cloned())
    }

    async fn list_vaults(&self, workspace_id: &str) -> Result<Vec<ManagedVault>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .vaults
            .values()
            .filter(|vault| vault.workspace_id == workspace_id)
            .cloned()
            .collect())
    }

    async fn delete_vault(&self, id: &str) -> Result<bool, CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        let removed = state.vaults.remove(id).is_some();
        if removed {
            state
                .vault_credentials
                .retain(|_, credential| credential.vault_id != id);
        }
        Ok(removed)
    }

    async fn put_vault_credential(
        &self,
        credential: ManagedVaultCredential,
    ) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .vault_credentials
            .insert(credential.id.clone(), credential);
        Ok(())
    }

    async fn get_vault_credential(
        &self,
        id: &str,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .vault_credentials
            .get(id)
            .cloned())
    }

    async fn get_vault_credential_by_source(
        &self,
        source_id: &CredentialSourceId,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .vault_credentials
            .values()
            .find(|credential| &credential.source_id == source_id)
            .cloned())
    }

    async fn list_vault_credentials(
        &self,
        vault_id: &str,
    ) -> Result<Vec<ManagedVaultCredential>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .vault_credentials
            .values()
            .filter(|credential| credential.vault_id == vault_id)
            .cloned()
            .collect())
    }

    async fn delete_vault_credential(&self, id: &str) -> Result<bool, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .vault_credentials
            .remove(id)
            .is_some())
    }
}
