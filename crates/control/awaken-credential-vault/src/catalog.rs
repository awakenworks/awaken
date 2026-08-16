//! Secret-free Managed Vault catalog.
//!
//! The catalog owns the management projection that is not part of a
//! [`CredentialSource`](crate::CredentialSource). Secret material remains owned
//! exclusively by [`SecretStore`](crate::SecretStore), while a credential entry
//! points at the exact source aggregate behind it.

use std::collections::BTreeMap;

use awaken_credential_contract::{CredentialSourceId, TokenEndpointAuth};

use crate::CredentialError;

pub const MAX_CREDENTIALS_PER_MANAGED_VAULT: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum ManagedCredentialAdmissionError {
    #[error("Managed credential workspace does not match its authority")]
    WorkspaceMismatch,
    #[error("Managed credential parent Vault is unavailable or archived")]
    VaultUnavailable,
    #[error("Managed Vault credential limit reached")]
    LimitReached,
    #[error("Managed environment credential key `{0}` is already active")]
    DuplicateEnvironmentKey(String),
    #[error("Managed MCP credential URL must be an absolute HTTP(S) URL")]
    InvalidMcpUrl,
    #[error(transparent)]
    Store(#[from] CredentialError),
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedVaultMutationError {
    #[error("Managed Vault is unavailable in this workspace")]
    NotFound,
    #[error("Managed Vault changed concurrently")]
    RevisionConflict,
    #[error("Managed Vault revision is exhausted")]
    RevisionExhausted,
    #[error(transparent)]
    Store(#[from] CredentialError),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedVault {
    pub id: String,
    pub workspace_id: String,
    pub display_name: String,
    pub metadata: BTreeMap<String, String>,
    pub archived_at: Option<String>,
    /// Monotonic aggregate revision. Legacy persisted rows deserialize as zero
    /// and enter the same compare-and-swap protocol on their first mutation.
    #[serde(default)]
    pub revision: u64,
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

/// Admit a Vault write only when the request authority owns both the proposed
/// row and any durable row already carrying that id. Generic equality keeps the
/// production String decision small enough to exhaust over a bounded identity
/// domain in Kani without introducing a parallel proof-only implementation.
fn managed_vault_workspace_admitted<T: PartialEq + ?Sized>(
    authority: &T,
    proposed_owner: &T,
    existing_owner: Option<&T>,
) -> bool {
    authority == proposed_owner
        && existing_owner.is_none_or(|existing_owner| existing_owner == authority)
}

/// Admit a Managed credential write only when its row, parent Vault, and any
/// existing child with the same id all carry the exact request Workspace.
fn managed_credential_workspace_admitted<T: PartialEq + ?Sized>(
    authority: &T,
    proposed_owner: &T,
    parent_owner: Option<&T>,
    existing_owner: Option<&T>,
) -> bool {
    authority == proposed_owner
        && parent_owner.is_some_and(|parent_owner| parent_owner == authority)
        && existing_owner.is_none_or(|existing_owner| existing_owner == authority)
}

/// Pure aggregate admission rule shared by every store implementation and its
/// bounded proof. The repository computes the facts while holding the parent
/// Vault write lock, so this decision and the child insert are one transaction.
#[must_use]
const fn managed_credential_insert_admitted(
    workspace_matches: bool,
    vault_active: bool,
    credential_count: usize,
    duplicate_environment_key: bool,
) -> bool {
    workspace_matches
        && vault_active
        && credential_count < MAX_CREDENTIALS_PER_MANAGED_VAULT
        && !duplicate_environment_key
}

/// A replacement is admitted only for the exact observed revision and must
/// advance it by one. Keeping this decision pure gives every store one rule and
/// makes stale-write rejection exhaustively checkable.
#[must_use]
const fn managed_vault_replacement_admitted(
    stored_revision: u64,
    expected_revision: u64,
    proposed_revision: u64,
) -> bool {
    stored_revision == expected_revision
        && match expected_revision.checked_add(1) {
            Some(next) => proposed_revision == next,
            None => false,
        }
}

pub fn admit_managed_vault_replacement(
    workspace_id: &str,
    current: Option<&ManagedVault>,
    expected_revision: u64,
    proposed: &ManagedVault,
) -> Result<(), ManagedVaultMutationError> {
    let current = current
        .filter(|current| {
            current.workspace_id == workspace_id
                && proposed.workspace_id == workspace_id
                && current.id == proposed.id
        })
        .ok_or(ManagedVaultMutationError::NotFound)?;
    let next = expected_revision
        .checked_add(1)
        .ok_or(ManagedVaultMutationError::RevisionExhausted)?;
    if current.revision != expected_revision || proposed.revision != next {
        return Err(ManagedVaultMutationError::RevisionConflict);
    }
    debug_assert!(managed_vault_replacement_admitted(
        current.revision,
        expected_revision,
        proposed.revision
    ));
    Ok(())
}

fn duplicate_environment_key<'a>(
    proposed: &ManagedVaultCredential,
    existing: impl IntoIterator<Item = &'a ManagedVaultCredential>,
) -> Option<String> {
    let ManagedCredentialAuth::EnvironmentVariable { secret_name, .. } = &proposed.auth else {
        return None;
    };
    existing
        .into_iter()
        .any(|credential| {
            credential.archived_at.is_none()
                && matches!(
                    &credential.auth,
                    ManagedCredentialAuth::EnvironmentVariable {
                        secret_name: current,
                        ..
                    } if current == secret_name
                )
        })
        .then(|| secret_name.clone())
}

pub fn admit_managed_credential_insert(
    workspace_id: &str,
    vault: Option<&ManagedVault>,
    existing: &[ManagedVaultCredential],
    proposed: &ManagedVaultCredential,
) -> Result<(), ManagedCredentialAdmissionError> {
    if proposed.workspace_id != workspace_id {
        return Err(ManagedCredentialAdmissionError::WorkspaceMismatch);
    }
    if let ManagedCredentialAuth::StaticBearer { mcp_server_url }
    | ManagedCredentialAuth::McpOauth { mcp_server_url, .. } = &proposed.auth
    {
        let valid = url::Url::parse(mcp_server_url).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
        });
        if !valid {
            return Err(ManagedCredentialAdmissionError::InvalidMcpUrl);
        }
    }
    let vault_active = vault.is_some_and(|vault| {
        vault.workspace_id == workspace_id
            && vault.id == proposed.vault_id
            && vault.archived_at.is_none()
    });
    let duplicate = duplicate_environment_key(proposed, existing);
    if managed_credential_insert_admitted(true, vault_active, existing.len(), duplicate.is_some()) {
        return Ok(());
    }
    if !vault_active {
        Err(ManagedCredentialAdmissionError::VaultUnavailable)
    } else if existing.len() >= MAX_CREDENTIALS_PER_MANAGED_VAULT {
        Err(ManagedCredentialAdmissionError::LimitReached)
    } else {
        Err(ManagedCredentialAdmissionError::DuplicateEnvironmentKey(
            duplicate.expect("duplicate admission fact"),
        ))
    }
}

/// One durable authority for the secret-free Managed Vault projection.
/// Implementations must cascade `delete_vault` to its credential entries.
#[async_trait::async_trait]
pub trait ManagedVaultRepo: Send + Sync {
    async fn put_vault(
        &self,
        workspace_id: &str,
        vault: ManagedVault,
    ) -> Result<(), CredentialError>;
    /// Replace an existing Vault with optimistic concurrency control. The
    /// repository compares and writes while holding one aggregate lock.
    async fn replace_vault(
        &self,
        workspace_id: &str,
        expected_revision: u64,
        vault: ManagedVault,
    ) -> Result<(), ManagedVaultMutationError>;
    async fn get_vault(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<ManagedVault>, CredentialError>;
    async fn list_vaults(&self, workspace_id: &str) -> Result<Vec<ManagedVault>, CredentialError>;
    async fn delete_vault(&self, workspace_id: &str, id: &str) -> Result<bool, CredentialError>;

    async fn put_vault_credential(
        &self,
        workspace_id: &str,
        credential: ManagedVaultCredential,
    ) -> Result<(), CredentialError>;
    /// Insert a newly-authored child while atomically enforcing the Vault
    /// aggregate's active/count/unique-key invariants.
    async fn insert_vault_credential(
        &self,
        workspace_id: &str,
        credential: ManagedVaultCredential,
    ) -> Result<(), ManagedCredentialAdmissionError>;
    async fn get_vault_credential(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError>;
    async fn get_vault_credential_by_source(
        &self,
        workspace_id: &str,
        source_id: &CredentialSourceId,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError>;
    async fn list_vault_credentials(
        &self,
        workspace_id: &str,
        vault_id: &str,
    ) -> Result<Vec<ManagedVaultCredential>, CredentialError>;
    async fn delete_vault_credential(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<bool, CredentialError>;
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
impl ManagedVaultRepo for crate::repo::InMemoryCredentialRepo {
    async fn put_vault(
        &self,
        workspace_id: &str,
        vault: ManagedVault,
    ) -> Result<(), CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        let existing_owner = state
            .vaults
            .get(&vault.id)
            .map(|current| current.workspace_id.as_str());
        if !managed_vault_workspace_admitted(
            workspace_id,
            vault.workspace_id.as_str(),
            existing_owner,
        ) {
            let message = if vault.workspace_id != workspace_id {
                "Managed Vault workspace does not match its authority"
            } else {
                "Managed Vault id belongs to another workspace"
            };
            return Err(CredentialError::InvalidSource(message.into()));
        }
        state.vaults.insert(vault.id.clone(), vault);
        Ok(())
    }

    async fn replace_vault(
        &self,
        workspace_id: &str,
        expected_revision: u64,
        vault: ManagedVault,
    ) -> Result<(), ManagedVaultMutationError> {
        let mut state = self.state.lock().expect("credential repo");
        admit_managed_vault_replacement(
            workspace_id,
            state.vaults.get(&vault.id),
            expected_revision,
            &vault,
        )?;
        state.vaults.insert(vault.id.clone(), vault);
        Ok(())
    }

    async fn get_vault(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<ManagedVault>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .vaults
            .get(id)
            .filter(|vault| vault.workspace_id == workspace_id)
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

    async fn delete_vault(&self, workspace_id: &str, id: &str) -> Result<bool, CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        let removed = state
            .vaults
            .get(id)
            .is_some_and(|vault| vault.workspace_id == workspace_id)
            && state.vaults.remove(id).is_some();
        if removed {
            state.vault_credentials.retain(|_, credential| {
                credential.workspace_id != workspace_id || credential.vault_id != id
            });
        }
        Ok(removed)
    }

    async fn put_vault_credential(
        &self,
        workspace_id: &str,
        credential: ManagedVaultCredential,
    ) -> Result<(), CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        let parent_owner = state
            .vaults
            .get(&credential.vault_id)
            .map(|vault| vault.workspace_id.as_str());
        let existing_owner = state
            .vault_credentials
            .get(&credential.id)
            .map(|current| current.workspace_id.as_str());
        if !managed_credential_workspace_admitted(
            workspace_id,
            credential.workspace_id.as_str(),
            parent_owner,
            existing_owner,
        ) {
            let message = if credential.workspace_id != workspace_id {
                "Managed credential workspace does not match its authority"
            } else if parent_owner != Some(workspace_id) {
                "Managed credential parent Vault is unavailable in this workspace"
            } else {
                "Managed credential id belongs to another workspace"
            };
            return Err(CredentialError::InvalidSource(message.into()));
        }
        state
            .vault_credentials
            .insert(credential.id.clone(), credential);
        Ok(())
    }

    async fn insert_vault_credential(
        &self,
        workspace_id: &str,
        credential: ManagedVaultCredential,
    ) -> Result<(), ManagedCredentialAdmissionError> {
        let mut state = self.state.lock().expect("credential repo");
        let vault = state.vaults.get(&credential.vault_id);
        let existing = state
            .vault_credentials
            .values()
            .filter(|current| {
                current.workspace_id == workspace_id && current.vault_id == credential.vault_id
            })
            .cloned()
            .collect::<Vec<_>>();
        admit_managed_credential_insert(workspace_id, vault, &existing, &credential)?;
        if state.vault_credentials.contains_key(&credential.id) {
            return Err(ManagedCredentialAdmissionError::Store(
                CredentialError::InvalidSource("Managed credential id already exists".into()),
            ));
        }
        state
            .vault_credentials
            .insert(credential.id.clone(), credential);
        Ok(())
    }

    async fn get_vault_credential(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .vault_credentials
            .get(id)
            .filter(|credential| credential.workspace_id == workspace_id)
            .cloned())
    }

    async fn get_vault_credential_by_source(
        &self,
        workspace_id: &str,
        source_id: &CredentialSourceId,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .vault_credentials
            .values()
            .find(|credential| {
                credential.workspace_id == workspace_id && &credential.source_id == source_id
            })
            .cloned())
    }

    async fn list_vault_credentials(
        &self,
        workspace_id: &str,
        vault_id: &str,
    ) -> Result<Vec<ManagedVaultCredential>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .vault_credentials
            .values()
            .filter(|credential| {
                credential.workspace_id == workspace_id && credential.vault_id == vault_id
            })
            .cloned()
            .collect())
    }

    async fn delete_vault_credential(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<bool, CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        if !state
            .vault_credentials
            .get(id)
            .is_some_and(|credential| credential.workspace_id == workspace_id)
        {
            return Ok(false);
        }
        Ok(state.vault_credentials.remove(id).is_some())
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn managed_vault_workspace_admission_is_exact_and_non_widening() {
        let authority = kani::any::<u8>();
        let proposed_owner = kani::any::<u8>();
        let existing_owner = kani::any::<u8>();
        let has_existing = kani::any::<bool>();
        let existing_owner = has_existing.then_some(&existing_owner);

        let admitted =
            managed_vault_workspace_admitted(&authority, &proposed_owner, existing_owner);
        assert_eq!(
            admitted,
            authority == proposed_owner
                && existing_owner.is_none_or(|existing_owner| *existing_owner == authority)
        );
        if admitted {
            assert_eq!(proposed_owner, authority);
            if let Some(existing_owner) = existing_owner {
                assert_eq!(*existing_owner, authority);
            }
        }
    }

    #[kani::proof]
    fn managed_credential_workspace_admission_requires_exact_parent_and_owner() {
        let authority = kani::any::<u8>();
        let proposed_owner = kani::any::<u8>();
        let parent_owner = kani::any::<u8>();
        let existing_owner = kani::any::<u8>();
        let has_parent = kani::any::<bool>();
        let has_existing = kani::any::<bool>();
        let parent_owner = has_parent.then_some(&parent_owner);
        let existing_owner = has_existing.then_some(&existing_owner);

        let admitted = managed_credential_workspace_admitted(
            &authority,
            &proposed_owner,
            parent_owner,
            existing_owner,
        );
        assert_eq!(
            admitted,
            authority == proposed_owner
                && parent_owner.is_some_and(|parent_owner| *parent_owner == authority)
                && existing_owner.is_none_or(|existing_owner| *existing_owner == authority)
        );
        if admitted {
            assert_eq!(proposed_owner, authority);
            assert_eq!(parent_owner.copied(), Some(authority));
            if let Some(existing_owner) = existing_owner {
                assert_eq!(*existing_owner, authority);
            }
        }
    }

    #[kani::proof]
    fn managed_credential_insert_requires_every_aggregate_invariant() {
        let workspace_matches = kani::any();
        let vault_active = kani::any();
        let credential_count = kani::any::<usize>();
        let duplicate_environment_key = kani::any();
        let admitted = managed_credential_insert_admitted(
            workspace_matches,
            vault_active,
            credential_count,
            duplicate_environment_key,
        );
        assert_eq!(
            admitted,
            workspace_matches
                && vault_active
                && credential_count < MAX_CREDENTIALS_PER_MANAGED_VAULT
                && !duplicate_environment_key
        );
    }

    #[kani::proof]
    fn managed_vault_replacement_requires_exact_revision_successor() {
        let stored_revision = kani::any();
        let expected_revision = kani::any();
        let proposed_revision = kani::any();
        let admitted = managed_vault_replacement_admitted(
            stored_revision,
            expected_revision,
            proposed_revision,
        );
        assert_eq!(
            admitted,
            stored_revision == expected_revision
                && expected_revision
                    .checked_add(1)
                    .is_some_and(|next| proposed_revision == next)
        );
        if admitted {
            assert_eq!(stored_revision, expected_revision);
            assert_eq!(proposed_revision, expected_revision + 1);
        }
    }
}
