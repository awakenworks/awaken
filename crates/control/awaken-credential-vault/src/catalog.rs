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
    #[error("Managed Vault lifecycle transition is not allowed")]
    InvalidLifecycle,
    #[error(transparent)]
    Store(#[from] CredentialError),
}

/// Durable root-delete phase. A requested deletion is already unavailable to
/// new child mutations; completion is delayed until every child has published
/// its tombstone and every resulting rollout has been acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedVaultDeletionPhase {
    Requested,
    Completed,
}

/// Stable, secret-free identity for one retryable Managed Vault deletion.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedVaultDeletion {
    pub operation_id: String,
    pub requested_at: String,
    pub phase: ManagedVaultDeletionPhase,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedCredentialMutationError {
    #[error("Managed credential is unavailable in this workspace and Vault")]
    NotFound,
    #[error("Managed credential changed concurrently")]
    RevisionConflict,
    #[error("Managed credential revision is exhausted")]
    RevisionExhausted,
    #[error("Managed credential lifecycle transition is not allowed")]
    InvalidLifecycle,
    #[error(transparent)]
    Admission(#[from] ManagedCredentialAdmissionError),
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
    /// Durable delete intent and tombstone. Legacy rows have no deletion.
    /// Physical purge is a separate maintenance concern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion: Option<ManagedVaultDeletion>,
    /// Monotonic aggregate revision. Legacy persisted rows deserialize as zero
    /// and enter the same compare-and-swap protocol on their first mutation.
    #[serde(default)]
    pub revision: u64,
}

impl ManagedVault {
    #[must_use]
    pub const fn accepts_child_mutation(&self) -> bool {
        self.archived_at.is_none() && self.deletion.is_none()
    }

    #[must_use]
    pub const fn deletion_requested(&self) -> bool {
        matches!(
            self.deletion.as_ref(),
            Some(ManagedVaultDeletion {
                phase: ManagedVaultDeletionPhase::Requested,
                ..
            })
        )
    }

    #[must_use]
    pub const fn is_deleted(&self) -> bool {
        matches!(
            self.deletion.as_ref(),
            Some(ManagedVaultDeletion {
                phase: ManagedVaultDeletionPhase::Completed,
                ..
            })
        )
    }
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

/// Management lifecycle of one Vault child. The timestamp belongs to the
/// transition, so invalid combinations such as both archived and deleted are
/// unrepresentable. `Deleted` is absorbing; physical purge is a separate GC
/// concern and never part of the public delete command.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ManagedCredentialLifecycle {
    Active,
    Archived { at: String },
    Deleted { at: String },
}

impl ManagedCredentialLifecycle {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }

    #[must_use]
    pub const fn is_deleted(&self) -> bool {
        matches!(self, Self::Deleted { .. })
    }

    #[must_use]
    pub fn archived_at(&self) -> Option<&str> {
        match self {
            Self::Active => None,
            Self::Archived { at } | Self::Deleted { at } => Some(at),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ManagedVaultCredential {
    pub id: String,
    pub vault_id: String,
    pub workspace_id: String,
    pub source_id: CredentialSourceId,
    pub auth: ManagedCredentialAuth,
    pub metadata: BTreeMap<String, String>,
    pub display_name: Option<String>,
    /// Monotonic entity revision used together with the Source version as one
    /// publication fence. Newly-authored rows start at one; legacy JSON rows
    /// decode as zero and enter CAS on their first mutation.
    pub revision: u64,
    pub lifecycle: ManagedCredentialLifecycle,
}

#[derive(serde::Deserialize)]
struct ManagedVaultCredentialWire {
    id: String,
    vault_id: String,
    workspace_id: String,
    source_id: CredentialSourceId,
    auth: ManagedCredentialAuth,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    lifecycle: Option<ManagedCredentialLifecycle>,
    /// Compatibility-only fields from the pre-lifecycle persisted shape.
    #[serde(default)]
    archived_at: Option<String>,
    #[serde(default)]
    deleted_at: Option<String>,
}

impl<'de> serde::Deserialize<'de> for ManagedVaultCredential {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ManagedVaultCredentialWire::deserialize(deserializer)?;
        let lifecycle = wire.lifecycle.unwrap_or({
            if let Some(at) = wire.deleted_at {
                ManagedCredentialLifecycle::Deleted { at }
            } else if let Some(at) = wire.archived_at {
                ManagedCredentialLifecycle::Archived { at }
            } else {
                ManagedCredentialLifecycle::Active
            }
        });
        Ok(Self {
            id: wire.id,
            vault_id: wire.vault_id,
            workspace_id: wire.workspace_id,
            source_id: wire.source_id,
            auth: wire.auth,
            metadata: wire.metadata,
            display_name: wire.display_name,
            revision: wire.revision,
            lifecycle,
        })
    }
}

/// Admit a Vault write only when the request authority owns both the proposed
/// row and any durable row already carrying that id. Generic equality keeps the
/// production String decision small enough to exhaust over a bounded identity
/// domain in Kani without introducing a parallel proof-only implementation.
#[cfg(any(kani, test, feature = "test-support"))]
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
#[cfg(kani)]
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

#[must_use]
const fn managed_credential_replacement_admitted(
    stored_revision: u64,
    expected_revision: u64,
    proposed_revision: u64,
    binding_matches: bool,
    lifecycle_allowed: bool,
) -> bool {
    binding_matches
        && lifecycle_allowed
        && stored_revision == expected_revision
        && match expected_revision.checked_add(1) {
            Some(next) => proposed_revision == next,
            None => false,
        }
}

#[must_use]
const fn managed_credential_lifecycle_transition_allowed(
    before: &ManagedCredentialLifecycle,
    after: &ManagedCredentialLifecycle,
) -> bool {
    matches!(
        (before, after),
        (
            ManagedCredentialLifecycle::Active,
            ManagedCredentialLifecycle::Active
        ) | (
            ManagedCredentialLifecycle::Active,
            ManagedCredentialLifecycle::Archived { .. }
        ) | (
            ManagedCredentialLifecycle::Active,
            ManagedCredentialLifecycle::Deleted { .. }
        ) | (
            ManagedCredentialLifecycle::Archived { .. },
            ManagedCredentialLifecycle::Archived { .. }
        ) | (
            ManagedCredentialLifecycle::Archived { .. },
            ManagedCredentialLifecycle::Deleted { .. }
        ) | (
            ManagedCredentialLifecycle::Deleted { .. },
            ManagedCredentialLifecycle::Deleted { .. }
        )
    )
}

pub fn admit_managed_credential_replacement(
    workspace_id: &str,
    current: Option<&ManagedVaultCredential>,
    expected_revision: u64,
    proposed: &ManagedVaultCredential,
) -> Result<(), ManagedCredentialMutationError> {
    let current = current
        .filter(|current| {
            current.workspace_id == workspace_id
                && proposed.workspace_id == workspace_id
                && current.id == proposed.id
                && current.vault_id == proposed.vault_id
                && current.source_id == proposed.source_id
        })
        .ok_or(ManagedCredentialMutationError::NotFound)?;
    let next = expected_revision
        .checked_add(1)
        .ok_or(ManagedCredentialMutationError::RevisionExhausted)?;
    if !managed_credential_lifecycle_transition_allowed(&current.lifecycle, &proposed.lifecycle) {
        return Err(ManagedCredentialMutationError::InvalidLifecycle);
    }
    if current.revision != expected_revision || proposed.revision != next {
        return Err(ManagedCredentialMutationError::RevisionConflict);
    }
    debug_assert!(managed_credential_replacement_admitted(
        current.revision,
        expected_revision,
        proposed.revision,
        true,
        true,
    ));
    Ok(())
}

#[must_use]
const fn managed_vault_deletion_transition_admitted(
    current: Option<ManagedVaultDeletionPhase>,
    proposed: Option<ManagedVaultDeletionPhase>,
    exact_identity: bool,
) -> bool {
    matches!(
        (current, proposed),
        (None, None)
            | (None, Some(ManagedVaultDeletionPhase::Requested))
            | (
                Some(ManagedVaultDeletionPhase::Requested),
                Some(ManagedVaultDeletionPhase::Completed)
            )
    ) && (current.is_none() || exact_identity)
}

fn managed_vault_deletion_identity_matches(
    current: Option<&ManagedVaultDeletion>,
    proposed: Option<&ManagedVaultDeletion>,
) -> bool {
    match (current, proposed) {
        (None, None) | (None, Some(_)) => true,
        (Some(current), Some(proposed)) => {
            current.operation_id == proposed.operation_id
                && current.requested_at == proposed.requested_at
        }
        (Some(_), None) => false,
    }
}

/// Construct the one durable delete request for a Vault root. Replays return
/// the exact existing request without advancing revision.
pub fn request_managed_vault_deletion(
    current: &ManagedVault,
    requested_at: String,
) -> Result<(ManagedVault, bool), ManagedVaultMutationError> {
    if current.deletion.is_some() {
        return Ok((current.clone(), false));
    }
    let revision = current
        .revision
        .checked_add(1)
        .ok_or(ManagedVaultMutationError::RevisionExhausted)?;
    let operation_id = format!("managed-vault-delete:{}:{}", current.id, current.revision);
    let mut proposed = current.clone();
    proposed
        .archived_at
        .get_or_insert_with(|| requested_at.clone());
    proposed.deletion = Some(ManagedVaultDeletion {
        operation_id,
        requested_at,
        phase: ManagedVaultDeletionPhase::Requested,
    });
    proposed.revision = revision;
    Ok((proposed, true))
}

/// Complete the root tombstone only after the application service has observed
/// every child tombstone and no pending rollout for this Vault.
pub fn complete_managed_vault_deletion(
    current: &ManagedVault,
) -> Result<(ManagedVault, bool), ManagedVaultMutationError> {
    let Some(deletion) = current.deletion.as_ref() else {
        return Err(ManagedVaultMutationError::InvalidLifecycle);
    };
    if deletion.phase == ManagedVaultDeletionPhase::Completed {
        return Ok((current.clone(), false));
    }
    let revision = current
        .revision
        .checked_add(1)
        .ok_or(ManagedVaultMutationError::RevisionExhausted)?;
    let mut proposed = current.clone();
    proposed.revision = revision;
    proposed
        .deletion
        .as_mut()
        .expect("deletion was checked")
        .phase = ManagedVaultDeletionPhase::Completed;
    Ok((proposed, true))
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
    let exact_deletion_identity = managed_vault_deletion_identity_matches(
        current.deletion.as_ref(),
        proposed.deletion.as_ref(),
    );
    if !managed_vault_deletion_transition_admitted(
        current.deletion.as_ref().map(|deletion| deletion.phase),
        proposed.deletion.as_ref().map(|deletion| deletion.phase),
        exact_deletion_identity,
    ) {
        return Err(ManagedVaultMutationError::InvalidLifecycle);
    }
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
            credential.lifecycle.is_active()
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
    if proposed.workspace_id != workspace_id
        || proposed.revision != 1
        || !proposed.lifecycle.is_active()
    {
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
            && vault.accepts_child_mutation()
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

/// One durable authority for the secret-free Managed Vault projection. Public
/// lifecycle changes are revisioned replacements; physical purge is not part
/// of this application port.
#[async_trait::async_trait]
pub trait ManagedVaultRepo: Send + Sync {
    async fn insert_vault(
        &self,
        workspace_id: &str,
        vault: ManagedVault,
    ) -> Result<(), CredentialError>;
    /// Insert a stable system-owned Vault if absent and otherwise return the
    /// existing aggregate without resetting operator metadata or revision.
    async fn ensure_vault(
        &self,
        workspace_id: &str,
        vault: ManagedVault,
    ) -> Result<ManagedVault, CredentialError>;
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
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
impl ManagedVaultRepo for crate::repo::InMemoryCredentialRepo {
    async fn insert_vault(
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
        if existing_owner.is_some() {
            return Err(CredentialError::MutationConflict(
                "Managed Vault id already exists".into(),
            ));
        }
        state.vaults.insert(vault.id.clone(), vault);
        Ok(())
    }

    async fn ensure_vault(
        &self,
        workspace_id: &str,
        vault: ManagedVault,
    ) -> Result<ManagedVault, CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        if vault.workspace_id != workspace_id {
            return Err(CredentialError::InvalidSource(
                "Managed Vault workspace does not match its authority".into(),
            ));
        }
        match state.vaults.get(&vault.id) {
            Some(existing) if existing.workspace_id == workspace_id => Ok(existing.clone()),
            Some(_) => Err(CredentialError::InvalidSource(
                "Managed Vault id belongs to another workspace".into(),
            )),
            None => {
                state.vaults.insert(vault.id.clone(), vault.clone());
                Ok(vault)
            }
        }
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

    #[kani::proof]
    fn managed_vault_delete_is_monotonic_identity_bound_and_absorbing() {
        let current = match kani::any::<u8>() % 3 {
            0 => None,
            1 => Some(ManagedVaultDeletionPhase::Requested),
            _ => Some(ManagedVaultDeletionPhase::Completed),
        };
        let proposed = match kani::any::<u8>() % 3 {
            0 => None,
            1 => Some(ManagedVaultDeletionPhase::Requested),
            _ => Some(ManagedVaultDeletionPhase::Completed),
        };
        let exact_identity = kani::any::<bool>();
        let admitted =
            managed_vault_deletion_transition_admitted(current, proposed, exact_identity);

        if current == Some(ManagedVaultDeletionPhase::Completed) {
            assert!(!admitted);
        }
        if current == Some(ManagedVaultDeletionPhase::Requested) && admitted {
            assert_eq!(proposed, Some(ManagedVaultDeletionPhase::Completed));
            assert!(exact_identity);
        }
        if current.is_some() && proposed.is_none() {
            assert!(!admitted);
        }
    }

    #[kani::proof]
    fn managed_credential_replacement_requires_both_fences_and_exact_successor() {
        let stored_revision = kani::any();
        let expected_revision = kani::any();
        let proposed_revision = kani::any();
        let binding_matches = kani::any();
        let lifecycle_allowed = kani::any();
        let admitted = managed_credential_replacement_admitted(
            stored_revision,
            expected_revision,
            proposed_revision,
            binding_matches,
            lifecycle_allowed,
        );
        assert_eq!(
            admitted,
            binding_matches
                && lifecycle_allowed
                && stored_revision == expected_revision
                && expected_revision
                    .checked_add(1)
                    .is_some_and(|next| proposed_revision == next)
        );
    }

    #[kani::proof]
    fn deleted_managed_credential_is_absorbing() {
        let after = if kani::any::<bool>() {
            ManagedCredentialLifecycle::Active
        } else if kani::any::<bool>() {
            ManagedCredentialLifecycle::Archived { at: String::new() }
        } else {
            ManagedCredentialLifecycle::Deleted { at: String::new() }
        };
        assert_eq!(
            managed_credential_lifecycle_transition_allowed(
                &ManagedCredentialLifecycle::Deleted { at: String::new() },
                &after,
            ),
            matches!(after, ManagedCredentialLifecycle::Deleted { .. })
        );
    }
}
