// @formal entrypoint credential.injection_location_has_exactly_three_usable_states_and_partial_updates_preserve_validity
// @formal entrypoint credential.managed_deleted_lifecycle_is_absorbing
// @formal entrypoint credential.managed_mutation_requires_exact_source_and_child_successor_fences
// @formal entrypoint credential.managed_vault_child_admission_requires_active_parent_capacity_and_unique_key
// @formal entrypoint credential.managed_vault_delete_fences_children_waits_for_rollout_and_is_absorbing
// @formal entrypoint credential.managed_vault_replacement_rejects_stale_and_non_successor_revisions
// @formal entrypoint credential.vault_workspace_authority_admission_is_exact_and_non_widening
//! Secret-free Managed Vault catalog.
//!
//! The catalog owns the management projection that is not part of a
//! [`CredentialSource`](crate::CredentialSource). Secret material remains owned
//! exclusively by [`SecretStore`](crate::SecretStore), while a credential entry
//! points at the exact source aggregate behind it.

use std::collections::BTreeMap;

use awaken_agent_contract::credential::TokenEndpointAuth;
use awaken_credential_contract::CredentialSourceId;

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
    #[error(
        "Managed credential mutation failed ({primary}); abort reconciliation also failed ({cleanup})"
    )]
    Compensation { primary: String, cleanup: String },
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
    /// RFC 3339 instant the Vault was created. Stamped once from the owner's
    /// clock and immutable across every replacement. Rows persisted before this
    /// field existed are backfilled by the store migration from the row's own
    /// durable insert time.
    pub created_at: String,
    /// RFC 3339 instant of the Vault's last committed mutation (equal to
    /// `created_at` until the first one).
    pub updated_at: String,
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

/// Effective egress locations for an environment-variable credential. The
/// public API rejects `header=false, body=false`; representing only the three
/// usable states keeps that invariant durable and impossible to bypass during
/// projection, recovery, or partial update.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedCredentialInjectionLocation {
    HeaderOnly,
    BodyOnly,
    #[default]
    HeaderAndBody,
}

impl ManagedCredentialInjectionLocation {
    #[must_use]
    pub const fn from_flags(header: bool, body: bool) -> Option<Self> {
        match (header, body) {
            (true, false) => Some(Self::HeaderOnly),
            (false, true) => Some(Self::BodyOnly),
            (true, true) => Some(Self::HeaderAndBody),
            (false, false) => None,
        }
    }

    #[must_use]
    pub const fn header(self) -> bool {
        matches!(self, Self::HeaderOnly | Self::HeaderAndBody)
    }

    #[must_use]
    pub const fn body(self) -> bool {
        matches!(self, Self::BodyOnly | Self::HeaderAndBody)
    }

    #[must_use]
    pub const fn merge(self, header: Option<bool>, body: Option<bool>) -> Option<Self> {
        Self::from_flags(
            match header {
                Some(value) => value,
                None => self.header(),
            },
            match body {
                Some(value) => value,
                None => self.body(),
            },
        )
    }
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
        #[serde(default)]
        injection_location: ManagedCredentialInjectionLocation,
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
    /// RFC 3339 instant the credential was created; immutable after creation.
    /// Legacy rows are backfilled by the store migration from the row's own
    /// durable insert time.
    pub created_at: String,
    /// RFC 3339 instant of the credential's last committed mutation.
    pub updated_at: String,
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
    created_at: String,
    updated_at: String,
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
            created_at: wire.created_at,
            updated_at: wire.updated_at,
            revision: wire.revision,
            lifecycle,
        })
    }
}

impl ManagedVaultCredential {
    /// Whether `other` is the same logical child regardless of *when* it was
    /// authored. A retried command samples a fresh clock, so idempotent replay
    /// and pending-fact matching must compare content, never timestamps:
    /// `created_at`, `updated_at` and the lifecycle transition instant.
    #[must_use]
    pub fn same_logical_content(&self, other: &Self) -> bool {
        let untimed = |credential: &Self| Self {
            created_at: String::new(),
            updated_at: String::new(),
            lifecycle: match credential.lifecycle {
                ManagedCredentialLifecycle::Active => ManagedCredentialLifecycle::Active,
                ManagedCredentialLifecycle::Archived { .. } => {
                    ManagedCredentialLifecycle::Archived { at: String::new() }
                }
                ManagedCredentialLifecycle::Deleted { .. } => {
                    ManagedCredentialLifecycle::Deleted { at: String::new() }
                }
            },
            ..credential.clone()
        };
        untimed(self) == untimed(other)
    }
}

/// Admit a Vault write only when the request authority owns both the proposed
/// row and any durable row already carrying that id. Generic equality keeps the
/// production String decision small enough to exhaust over a bounded identity
/// domain in Kani without introducing a parallel proof-only implementation.
#[must_use]
pub fn managed_vault_workspace_admitted<T: PartialEq + ?Sized>(
    authority: &T,
    proposed_owner: &T,
    existing_owner: Option<&T>,
) -> bool {
    authority == proposed_owner
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
    // The creation instant is part of the child's birth fact: no replacement
    // may re-date it.
    if !managed_credential_lifecycle_transition_allowed(&current.lifecycle, &proposed.lifecycle)
        || current.created_at != proposed.created_at
    {
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
    proposed.updated_at.clone_from(&requested_at);
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
    // `created_at` is the Vault's birth fact and never changes after insert.
    if !managed_vault_deletion_transition_admitted(
        current.deletion.as_ref().map(|deletion| deletion.phase),
        proposed.deletion.as_ref().map(|deletion| deletion.phase),
        exact_deletion_identity,
    ) || current.created_at != proposed.created_at
    {
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
        let mut state = self.state()?;
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
        let mut state = self.state()?;
        if !managed_vault_workspace_admitted(workspace_id, vault.workspace_id.as_str(), None) {
            return Err(CredentialError::InvalidSource(
                "Managed Vault workspace does not match its authority".into(),
            ));
        }
        let existing = state.vaults.get(&vault.id);
        if !managed_vault_workspace_admitted(
            workspace_id,
            vault.workspace_id.as_str(),
            existing.map(|row| row.workspace_id.as_str()),
        ) {
            return Err(CredentialError::InvalidSource(
                "Managed Vault id belongs to another workspace".into(),
            ));
        }
        match existing {
            Some(existing) => Ok(existing.clone()),
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
        let mut state = self.state()?;
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
            .state()?
            .vaults
            .get(id)
            .filter(|vault| vault.workspace_id == workspace_id)
            .cloned())
    }

    async fn list_vaults(&self, workspace_id: &str) -> Result<Vec<ManagedVault>, CredentialError> {
        Ok(self
            .state()?
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
            .state()?
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
            .state()?
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
            .state()?
            .vault_credentials
            .values()
            .filter(|credential| {
                credential.workspace_id == workspace_id && credential.vault_id == vault_id
            })
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{ManagedCredentialAuth, ManagedCredentialInjectionLocation};

    /// Causal graph:
    /// legacy persisted row (field absent) -> serde recovery -> effective
    /// header+body injection -> stable reserialization. This is deliberately a
    /// persistence-boundary test rather than an HTTP test: a fresh router over
    /// the same typed in-memory repository cannot expose a missing serde
    /// default after process replacement.
    #[test]
    fn legacy_environment_auth_recovers_with_both_injection_locations() {
        let legacy = serde_json::json!({
            "type": "environment_variable",
            "secret_name": "ANTHROPIC_API_KEY",
            "networking": { "type": "unrestricted" }
        });

        let recovered: ManagedCredentialAuth =
            serde_json::from_value(legacy).expect("legacy auth must remain recoverable");
        let ManagedCredentialAuth::EnvironmentVariable {
            injection_location, ..
        } = recovered
        else {
            panic!("legacy environment auth changed variant");
        };

        assert_eq!(
            injection_location,
            ManagedCredentialInjectionLocation::HeaderAndBody
        );
        assert_eq!(
            serde_json::to_value(ManagedCredentialAuth::EnvironmentVariable {
                secret_name: "ANTHROPIC_API_KEY".into(),
                networking: super::ManagedCredentialNetworking::Unrestricted,
                injection_location,
            })
            .expect("recovered auth must serialize")["injection_location"],
            "header_and_body"
        );
    }

    /// Decision table over the complete durable state space. Each valid state
    /// must round-trip exactly; any fourth spelling must fail closed. Together
    /// with the Kani flag proof this covers both serialization and construction
    /// boundaries without encoding an invalid `header=false, body=false` state.
    #[test]
    fn injection_location_persistence_is_a_closed_three_state_enum() {
        for location in [
            ManagedCredentialInjectionLocation::HeaderOnly,
            ManagedCredentialInjectionLocation::BodyOnly,
            ManagedCredentialInjectionLocation::HeaderAndBody,
        ] {
            let encoded = serde_json::to_string(&location).expect("location must serialize");
            let decoded: ManagedCredentialInjectionLocation =
                serde_json::from_str(&encoded).expect("location must deserialize");
            assert_eq!(decoded, location);
        }

        assert!(
            serde_json::from_str::<ManagedCredentialInjectionLocation>("\"disabled\"").is_err(),
            "the durable model must reject an all-disabled escape state"
        );
    }
}

#[cfg(any(test, kani))]
mod workspace_proofs {
    use super::*;

    fn fixture() -> (ManagedVault, ManagedVaultCredential) {
        let vault = ManagedVault {
            id: "v".into(),
            workspace_id: "a".into(),
            display_name: String::new(),
            metadata: BTreeMap::new(),
            created_at: "t".into(),
            updated_at: "t".into(),
            archived_at: None,
            deletion: None,
            revision: 1,
        };
        let child = ManagedVaultCredential {
            id: "c".into(),
            vault_id: "v".into(),
            workspace_id: "a".into(),
            source_id: CredentialSourceId("s".into()),
            auth: ManagedCredentialAuth::EnvironmentVariable {
                secret_name: "K".into(),
                networking: ManagedCredentialNetworking::Unrestricted,
                injection_location: ManagedCredentialInjectionLocation::HeaderAndBody,
            },
            metadata: BTreeMap::new(),
            display_name: None,
            created_at: "t".into(),
            updated_at: "t".into(),
            revision: 1,
            lifecycle: ManagedCredentialLifecycle::Active,
        };
        (vault, child)
    }

    #[test]
    fn workspace_proof_fixture_is_admitted_by_production() {
        // Non-vacuity witness: exact owner/parent, active revision-one child,
        // live Vault and empty catalog must reach the production success arm.
        let (vault, child) = fixture();
        admit_managed_credential_insert("a", Some(&vault), &[], &child).unwrap();
    }

    /// Timestamp decision table. R1 a replacement that advances `updated_at`
    /// and keeps `created_at` is admitted for both aggregates; R2 re-dating
    /// `created_at` is rejected for both; R3 a deletion request stamps both
    /// `requested_at` and `updated_at` with the request instant and never
    /// touches `created_at`; R4 logical content ignores created/updated and
    /// lifecycle instants but still distinguishes content and lifecycle state.
    #[test]
    fn creation_instant_is_immutable_and_mutations_restamp_updated_at() {
        let (vault, child) = fixture();
        let mut updated_vault = vault.clone();
        updated_vault.revision = 2;
        updated_vault.updated_at = "u".into();
        admit_managed_vault_replacement("a", Some(&vault), 1, &updated_vault).expect("R1 vault");
        let mut updated_child = child.clone();
        updated_child.revision = 2;
        updated_child.updated_at = "u".into();
        admit_managed_credential_replacement("a", Some(&child), 1, &updated_child)
            .expect("R1 credential");

        updated_vault.created_at = "rewritten".into();
        assert!(matches!(
            admit_managed_vault_replacement("a", Some(&vault), 1, &updated_vault),
            Err(ManagedVaultMutationError::InvalidLifecycle)
        ));
        updated_child.created_at = "rewritten".into();
        assert!(matches!(
            admit_managed_credential_replacement("a", Some(&child), 1, &updated_child),
            Err(ManagedCredentialMutationError::InvalidLifecycle)
        ));

        let (requested, changed) = request_managed_vault_deletion(&vault, "d".into()).unwrap();
        assert!(changed);
        assert_eq!(requested.deletion.as_ref().unwrap().requested_at, "d");
        assert_eq!(requested.updated_at, "d", "R3");
        assert_eq!(requested.created_at, vault.created_at, "R3");
        admit_managed_vault_replacement("a", Some(&vault), 1, &requested).expect("R3 admitted");

        let mut retimed = child.clone();
        retimed.created_at = "other".into();
        retimed.updated_at = "other".into();
        assert!(child.same_logical_content(&retimed), "R4 times ignored");
        let archived_at = |at: &str| ManagedVaultCredential {
            lifecycle: ManagedCredentialLifecycle::Archived { at: at.into() },
            ..child.clone()
        };
        assert!(
            archived_at("x").same_logical_content(&archived_at("y")),
            "R4"
        );
        assert!(!child.same_logical_content(&archived_at("x")), "R4 state");
        let renamed = ManagedVaultCredential {
            display_name: Some("renamed".into()),
            ..retimed
        };
        assert!(!child.same_logical_content(&renamed), "R4 content");
    }

    #[cfg(kani)]
    /// @obligation credential.vault_workspace_authority_admission_is_exact_and_non_widening
    #[kani::proof]
    #[kani::unwind(4)]
    fn managed_credential_insert_checks_actual_workspace_and_parent() {
        // Causes: independent proposed/parent Workspace and parent-id equality,
        // parent presence, archive/deletion, child revision and lifecycle.
        // W1 invalid child owner/revision/lifecycle => WorkspaceMismatch first.
        // W2 valid child + absent/foreign/wrong-id/archived/deleting parent =>
        // VaultUnavailable. W3 valid child + exact live parent => success.
        // Constraints: identities a/b and v/x, fixed Environment auth, empty
        // siblings (no limit/duplicate-key cause), full u64 child revision.
        // Existing child-id CAS, arbitrary strings and URL parsing are outside
        // this actual-function proof and have separate adapter/validation tests.
        let (mut vault, mut child) = fixture();
        let proposed_matches = kani::any::<bool>();
        let parent_matches = kani::any::<bool>();
        let id_matches = kani::any::<bool>();
        let has_parent = kani::any::<bool>();
        let archived = kani::any::<bool>();
        let deletion = kani::any::<u8>() % 3;
        let lifecycle = kani::any::<u8>() % 3;
        child.workspace_id = if proposed_matches { "a" } else { "b" }.into();
        vault.workspace_id = if parent_matches { "a" } else { "b" }.into();
        vault.id = if id_matches { "v" } else { "x" }.into();
        vault.archived_at = archived.then(|| "t".into());
        vault.deletion = match deletion {
            0 => None,
            _ => Some(ManagedVaultDeletion {
                operation_id: "d".into(),
                requested_at: "t".into(),
                phase: if deletion == 1 {
                    ManagedVaultDeletionPhase::Requested
                } else {
                    ManagedVaultDeletionPhase::Completed
                },
            }),
        };
        child.revision = kani::any();
        child.lifecycle = match lifecycle {
            0 => ManagedCredentialLifecycle::Active,
            1 => ManagedCredentialLifecycle::Archived { at: "t".into() },
            _ => ManagedCredentialLifecycle::Deleted { at: "t".into() },
        };
        let result =
            admit_managed_credential_insert("a", has_parent.then_some(&vault), &[], &child);
        if !proposed_matches || child.revision != 1 || lifecycle != 0 {
            assert!(matches!(
                result,
                Err(ManagedCredentialAdmissionError::WorkspaceMismatch)
            ));
        } else if !has_parent || !parent_matches || !id_matches || archived || deletion != 0 {
            assert!(matches!(
                result,
                Err(ManagedCredentialAdmissionError::VaultUnavailable)
            ));
        } else {
            assert!(result.is_ok());
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// @obligation credential.vault_workspace_authority_admission_is_exact_and_non_widening
    #[kani::proof]
    fn managed_vault_workspace_admission_is_exact_and_non_widening() {
        // V1: proposed owner differs => deny regardless of durable owner.
        // V2: exact proposed owner + absent/exact durable owner => admit.
        // V3: exact proposed owner + foreign durable owner => deny.
        // All u8 identities and both presence states; SQL/String and concurrency
        // are cross-checked by the shared adapter table, not proved here.
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

    /// @obligation credential.managed_vault_child_admission_requires_active_parent_capacity_and_unique_key
    /// @proof-input-debt free_symbol managed_credential_insert_admitted duplicate_environment_key
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

    /// @obligation credential.managed_vault_replacement_rejects_stale_and_non_successor_revisions
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

    /// @obligation credential.managed_mutation_requires_exact_source_and_child_successor_fences
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
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const fn listed_location(location: ManagedCredentialInjectionLocation) -> usize {
        match location {
            ManagedCredentialInjectionLocation::HeaderOnly => 0,
            ManagedCredentialInjectionLocation::BodyOnly => 1,
            ManagedCredentialInjectionLocation::HeaderAndBody => 2,
        }
    }
    const EVERY_LOCATION: [ManagedCredentialInjectionLocation; 3] = [
        ManagedCredentialInjectionLocation::HeaderOnly,
        ManagedCredentialInjectionLocation::BodyOnly,
        ManagedCredentialInjectionLocation::HeaderAndBody,
    ];

    const fn listed_deletion_phase(phase: ManagedVaultDeletionPhase) -> usize {
        match phase {
            ManagedVaultDeletionPhase::Requested => 0,
            ManagedVaultDeletionPhase::Completed => 1,
        }
    }
    const EVERY_DELETION_PHASE: [ManagedVaultDeletionPhase; 2] = [
        ManagedVaultDeletionPhase::Requested,
        ManagedVaultDeletionPhase::Completed,
    ];

    #[test]
    fn every_list_names_each_variant_once_in_match_order() {
        for (index, value) in EVERY_LOCATION.into_iter().enumerate() {
            assert_eq!(listed_location(value), index);
        }
        for (index, value) in EVERY_DELETION_PHASE.into_iter().enumerate() {
            assert_eq!(listed_deletion_phase(value), index);
        }
    }

    /// @exhaustive bool(2) x bool(2) = 4
    /// @obligation credential.injection_location_has_exactly_three_usable_states_and_partial_updates_preserve_validity
    #[test]
    fn injection_location_represents_exactly_the_three_usable_states() {
        for header in [false, true] {
            for body in [false, true] {
                let location = ManagedCredentialInjectionLocation::from_flags(header, body);

                assert_eq!(location.is_some(), header || body);
                if let Some(location) = location {
                    assert_eq!(location.header(), header);
                    assert_eq!(location.body(), body);
                }
            }
        }
    }

    /// @exhaustive ManagedCredentialInjectionLocation(3) x Option<bool>(3) x Option<bool>(3) = 27
    /// @obligation credential.injection_location_has_exactly_three_usable_states_and_partial_updates_preserve_validity
    #[test]
    fn injection_location_partial_update_is_exact_and_never_allows_both_disabled() {
        let patches = [None, Some(false), Some(true)];
        for current in EVERY_LOCATION {
            for patch_header in patches {
                for patch_body in patches {
                    let expected_header = patch_header.unwrap_or_else(|| current.header());
                    let expected_body = patch_body.unwrap_or_else(|| current.body());
                    let merged = current.merge(patch_header, patch_body);

                    assert_eq!(merged.is_some(), expected_header || expected_body);
                    if let Some(merged) = merged {
                        assert_eq!(merged.header(), expected_header);
                        assert_eq!(merged.body(), expected_body);
                    }
                }
            }
        }
    }

    /// @exhaustive Option<ManagedVaultDeletionPhase>(3) x Option<ManagedVaultDeletionPhase>(3) x bool(2) = 18
    /// @obligation credential.managed_vault_delete_fences_children_waits_for_rollout_and_is_absorbing
    #[test]
    fn managed_vault_delete_is_monotonic_identity_bound_and_absorbing() {
        let phases = [
            None,
            Some(EVERY_DELETION_PHASE[0]),
            Some(EVERY_DELETION_PHASE[1]),
        ];
        for current in phases {
            for proposed in phases {
                for exact_identity in [false, true] {
                    let admitted = managed_vault_deletion_transition_admitted(
                        current,
                        proposed,
                        exact_identity,
                    );

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
            }
        }
    }

    /// @exhaustive ManagedCredentialLifecycle(3) = 3
    /// @obligation credential.managed_deleted_lifecycle_is_absorbing
    #[test]
    fn deleted_managed_credential_is_absorbing() {
        let listed = |lifecycle: &ManagedCredentialLifecycle| match lifecycle {
            ManagedCredentialLifecycle::Active => 0,
            ManagedCredentialLifecycle::Archived { .. } => 1,
            ManagedCredentialLifecycle::Deleted { .. } => 2,
        };
        let every_after = [
            ManagedCredentialLifecycle::Active,
            ManagedCredentialLifecycle::Archived { at: String::new() },
            ManagedCredentialLifecycle::Deleted { at: String::new() },
        ];
        for (index, after) in every_after.iter().enumerate() {
            assert_eq!(listed(after), index);
            assert_eq!(
                managed_credential_lifecycle_transition_allowed(
                    &ManagedCredentialLifecycle::Deleted { at: String::new() },
                    after,
                ),
                matches!(after, ManagedCredentialLifecycle::Deleted { .. })
            );
        }
    }
}
