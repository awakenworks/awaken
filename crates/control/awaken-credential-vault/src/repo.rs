//! The credential source repository port (ADR-0043) — stores the **secret-free**
//! [`CredentialSource`] rows (the sealed material lives behind [`SecretStore`], a
//! separate port). Its own `credential` migration scope is what lets the whole
//! domain be split into its own database/service (blast-radius isolation).

#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;
use std::collections::{BTreeMap, HashSet};
#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub use awaken_credential_contract::{ManagedCredentialOperation, ManagedCredentialRollout};

/// Local adoption progress. The cross-service HTTP adapter maps these two
/// states directly to 204 and 202, while command receipts serialize this same
/// enum; no second JSON state vocabulary is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedCredentialAdoptionProgress {
    Converged,
    Pending,
}

impl ManagedCredentialAdoptionProgress {
    #[must_use]
    pub const fn is_converged(self) -> bool {
        matches!(self, Self::Converged)
    }
}

/// Combine independent adoption targets without acknowledging a partial
/// rollout. The operation is associative, commutative, and idempotent over the
/// closed two-state progress vocabulary.
#[must_use]
pub const fn conjunctive_managed_credential_adoption_progress(
    left: ManagedCredentialAdoptionProgress,
    right: ManagedCredentialAdoptionProgress,
) -> ManagedCredentialAdoptionProgress {
    if left.is_converged() && right.is_converged() {
        ManagedCredentialAdoptionProgress::Converged
    } else {
        ManagedCredentialAdoptionProgress::Pending
    }
}

#[cfg(kani)]
#[kani::proof]
fn managed_rollout_ack_requires_converged_progress() {
    use ManagedCredentialAdoptionProgress::{Converged, Pending};

    assert!(!ManagedCredentialAdoptionProgress::Pending.is_converged());
    assert!(ManagedCredentialAdoptionProgress::Converged.is_converged());
    assert_eq!(
        conjunctive_managed_credential_adoption_progress(Converged, Converged),
        Converged
    );
    assert_eq!(
        conjunctive_managed_credential_adoption_progress(Converged, Pending),
        Pending
    );
    assert_eq!(
        conjunctive_managed_credential_adoption_progress(Pending, Converged),
        Pending
    );
    assert_eq!(
        conjunctive_managed_credential_adoption_progress(Pending, Pending),
        Pending
    );
}

/// Typed failures owned by the rollout target port. Pending convergence is a
/// successful state and therefore is not represented as an error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagedCredentialAdoptionError {
    #[error("credential adoption event is invalid: {0}")]
    InvalidEvent(String),
    #[error("credential adoption event id names a different payload")]
    IdentityCollision,
    #[error("credential adoption is unauthorized")]
    Unauthorized,
    #[error("credential adoption target is unavailable: {0}")]
    Unavailable(String),
}

#[cfg(any(test, feature = "test-support"))]
use crate::catalog::admit_managed_credential_insert;
use crate::catalog::{
    ManagedCredentialAdmissionError, ManagedCredentialLifecycle, ManagedCredentialMutationError,
    ManagedVault, ManagedVaultCredential, ManagedVaultRepo, admit_managed_credential_replacement,
    complete_managed_vault_deletion,
};
use crate::{
    CredentialCreateParams, CredentialError, CredentialKind, CredentialPool, CredentialPoolId,
    CredentialSource, CredentialSourceId, CredentialStatus, SecretStore, WorkerLocalBinding,
    prepare_source, prepare_source_with_id, validate_create_params,
};

mod application_mcp;
pub use application_mcp::{
    APPLICATION_MCP_PROVIDER_ID, ApplicationMcpBearerCommand, PreparedApplicationMcpBearerRotation,
    application_mcp_material_ref, application_mcp_operation_id,
    enter_or_rotate_application_mcp_bearer, prepare_application_mcp_bearer_rotation,
};

/// Secret-free write-ahead intent for create, rotate, disable, archive, or
/// revoke. `before = None` is creation; every other change compares the exact
/// previous revision before atomically publishing `after`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CredentialMutationIntent {
    #[serde(default)]
    pub before: Option<CredentialSource>,
    #[serde(alias = "source")]
    pub after: CredentialSource,
}

/// Parent-aggregate fence for terminal child commands. A Vault deletion may
/// issue only the absorbing child `Delete`; a public `Archive` remains an
/// ordinary child mutation and is rejected as soon as root deletion starts.
#[must_use]
pub fn managed_retirement_parent_admitted(
    operation: ManagedCredentialOperation,
    vault: Option<&ManagedVault>,
    credential: &ManagedVaultCredential,
) -> bool {
    let exact_parent = vault.is_some_and(|vault| {
        vault.workspace_id == credential.workspace_id && vault.id == credential.vault_id
    });
    managed_retirement_parent_fence(
        exact_parent,
        vault.is_some_and(ManagedVault::accepts_child_mutation),
        operation == ManagedCredentialOperation::Delete,
    )
}

#[must_use]
const fn managed_retirement_parent_fence(
    exact_parent: bool,
    parent_accepts_child_mutation: bool,
    is_delete: bool,
) -> bool {
    exact_parent && (is_delete || parent_accepts_child_mutation)
}

#[cfg(kani)]
#[kani::proof]
fn managed_vault_delete_fence_allows_only_absorbing_child_delete() {
    let exact_parent = kani::any::<bool>();
    let parent_accepts_child_mutation = kani::any::<bool>();
    let is_delete = kani::any::<bool>();
    let admitted =
        managed_retirement_parent_fence(exact_parent, parent_accepts_child_mutation, is_delete);
    assert_eq!(
        admitted,
        exact_parent && (is_delete || parent_accepts_child_mutation)
    );
    if admitted && !parent_accepts_child_mutation {
        assert!(exact_parent && is_delete);
    }
}

/// Durable process phase around the external SecretStore participant.
/// `Writing` is live-writer owned, `Ready` may atomically publish,
/// `Reclaiming` means database truth is already committed and only retired
/// material remains to be removed, and `ReclaimingAbort` means publication was
/// durably abandoned while unpublished material remains to be removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedCredentialMutationPhase {
    Writing,
    Ready,
    Reclaiming,
    ReclaimingAbort,
}

/// A writer has this long to finish the external material write before a
/// reconciler may fence it and take ownership. The next periodic pass performs
/// the actual recovery, so expiry does not itself mutate durable state.
pub const MANAGED_CREDENTIAL_WRITER_LEASE_MS: u64 = 120_000;

const MANAGED_CREDENTIAL_MUTATION_FORMAT_VERSION: u8 = 1;

/// Secret-free durable authority for every Managed Credential mutation.
/// Source and management child are one consistency pair. Plaintext never enters
/// this fact; only frozen references cross the recovery boundary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingManagedCredentialMutation {
    /// Serialization format discriminator. Kept private so ordinary callers
    /// cannot construct a durable authority with a struct literal; repository
    /// adapters deserialize legacy rows as version zero and may only recover
    /// them, never admit them as a new command.
    #[serde(default)]
    format_version: u8,
    pub operation_id: String,
    pub operation: ManagedCredentialOperation,
    #[serde(default)]
    pub before_source: Option<CredentialSource>,
    pub after_source: CredentialSource,
    #[serde(default)]
    pub before_credential: Option<ManagedVaultCredential>,
    pub after_credential: ManagedVaultCredential,
    pub phase: ManagedCredentialMutationPhase,
    /// Stable physical-effect namespace. Unlike writer ownership, this never
    /// changes when recovery fences an expired writer.
    #[serde(default)]
    attempt_id: String,
    /// Stable token of the process attempt allowed to advance `Writing`.
    /// Missing legacy fields deserialize as an unowned, expired writer and are
    /// claimed by recovery before any transition is attempted.
    #[serde(default)]
    pub writer_token: String,
    /// Monotonic fencing generation. A recovery takeover always increments it.
    #[serde(default)]
    pub writer_epoch: u64,
    /// Wall-clock lease fence used only to decide when takeover is allowed.
    #[serde(default)]
    pub writer_lease_expires_at_unix_ms: u64,
}

/// Construct the public secret-free adoption event from an exact committed
/// mutation. Creation has no online predecessor to replace and emits no event.
#[must_use]
pub fn managed_rollout_from_committed(
    pending: &PendingManagedCredentialMutation,
) -> Option<ManagedCredentialRollout> {
    (pending.operation != ManagedCredentialOperation::Create).then(|| ManagedCredentialRollout {
        id: pending.operation_id.clone(),
        workspace_id: pending.after_credential.workspace_id.clone(),
        vault_id: pending.after_credential.vault_id.clone(),
        credential_id: pending.after_credential.id.clone(),
        source_id: pending.after_source.id.clone(),
        source_version: u64::try_from(pending.after_source.version).unwrap_or_default(),
        credential_revision: pending.after_credential.revision,
        operation: pending.operation,
    })
}

#[must_use]
#[cfg(kani)]
const fn managed_rollout_publish_allowed(
    pair_committed: bool,
    is_create: bool,
    exact_source_fence: bool,
    exact_child_fence: bool,
) -> bool {
    pair_committed && !is_create && exact_source_fence && exact_child_fence
}

#[must_use]
const fn managed_creation_pair_admitted(
    workspace_matches: bool,
    source_matches: bool,
    vault_present: bool,
    credential_present: bool,
    source_active: bool,
    initial_revision: bool,
) -> bool {
    workspace_matches
        && source_matches
        && vault_present
        && credential_present
        && source_active
        && initial_revision
}

#[must_use]
const fn managed_mutation_operation_shape_allowed(
    operation: u8,
    before_child_deleted: bool,
    after_source_active: bool,
    after_source_archived: bool,
    after_child_active: bool,
    after_child_archived: bool,
    after_child_deleted: bool,
) -> bool {
    let operation_matches = match operation {
        0 => after_source_active && after_child_active,
        1 => after_source_active && after_child_active,
        2 => after_source_archived && after_child_archived,
        3 => after_source_archived && after_child_deleted,
        _ => false,
    };
    operation_matches && (!before_child_deleted || after_child_deleted)
}

#[must_use]
const fn managed_material_attempt_admitted(
    current_format: bool,
    writing: bool,
    writes_new_material: bool,
    valid_owner: bool,
    new_refs_bound_to_attempt: bool,
) -> bool {
    !current_format
        || (((!writing && !writes_new_material) || valid_owner)
            && (!writes_new_material || new_refs_bound_to_attempt))
}

#[must_use]
#[cfg(kani)]
const fn managed_creation_begin_allowed(source_published: bool, child_published: bool) -> bool {
    !source_published && !child_published
}

impl PendingManagedCredentialMutation {
    fn fresh_writer_lease() -> Result<(String, u64, u64), CredentialError> {
        let now_unix_ms = managed_credential_now_unix_ms()?;
        Ok((
            uuid::Uuid::new_v4().simple().to_string(),
            1,
            now_unix_ms
                .checked_add(MANAGED_CREDENTIAL_WRITER_LEASE_MS)
                .ok_or_else(|| {
                    CredentialError::MutationConflict(
                        "Managed credential writer lease deadline overflowed".into(),
                    )
                })?,
        ))
    }

    pub fn claim_after_expiry(
        &self,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<Self>, CredentialError> {
        if self.phase != ManagedCredentialMutationPhase::Writing
            || self.writer_lease_expires_at_unix_ms > now_unix_ms
        {
            return Ok(None);
        }
        if lease_expires_at_unix_ms <= now_unix_ms {
            return Err(CredentialError::MutationConflict(
                "Managed credential recovery lease must expire after claim time".into(),
            ));
        }
        let mut claimed = self.clone();
        // A legacy durable fact has no attempt namespace to migrate without
        // plaintext. Keep it in the legacy validation format for this one
        // recovery cycle while fencing its former writer with a fresh owner.
        // The fact is removed after commit/abort, so no new command can enter
        // through this compatibility path.
        if self.format_version != 0 {
            claimed.format_version = MANAGED_CREDENTIAL_MUTATION_FORMAT_VERSION;
        }
        claimed.writer_token = uuid::Uuid::new_v4().simple().to_string();
        claimed.writer_epoch = self.writer_epoch.checked_add(1).ok_or_else(|| {
            CredentialError::MutationConflict("Managed credential writer epoch is exhausted".into())
        })?;
        claimed.writer_lease_expires_at_unix_ms = lease_expires_at_unix_ms;
        Ok(Some(claimed))
    }

    #[must_use]
    fn has_valid_writer_owner(&self) -> bool {
        !self.writer_token.is_empty()
            && self.writer_epoch > 0
            && self.writer_lease_expires_at_unix_ms > 0
    }

    /// New commands must use the current closed construction format. Legacy
    /// rows are accepted only through recovery after deserialization.
    pub fn validate_for_begin(&self) -> Result<(), ManagedCredentialMutationError> {
        if self.format_version != MANAGED_CREDENTIAL_MUTATION_FORMAT_VERSION {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        self.validate()
    }

    /// Revalidate the complete durable command shape at every persistence
    /// boundary. Public serde fields are transport data, never authority.
    pub fn validate(&self) -> Result<(), ManagedCredentialMutationError> {
        if self.format_version > MANAGED_CREDENTIAL_MUTATION_FORMAT_VERSION
            || self.operation_id.trim().is_empty()
            || self.after_source.kind != CredentialKind::Vault
            || self.after_source.workspace_id != self.after_credential.workspace_id
            || self.after_source.id != self.after_credential.source_id
            || self.after_credential.vault_id.trim().is_empty()
            || self.after_credential.id.trim().is_empty()
        {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }

        let before_refs = self
            .before_source
            .iter()
            .flat_map(CredentialSource::material_refs)
            .collect::<HashSet<_>>();
        let writes_new_material = self
            .after_source
            .material_refs()
            .any(|reference| !before_refs.contains(reference));
        let suffix = format!(":attempt:{}", self.attempt_id);
        let new_refs_bound_to_attempt = !self.attempt_id.is_empty()
            && self
                .after_source
                .material_refs()
                .filter(|reference| !before_refs.contains(reference))
                .all(|reference| reference.0.ends_with(&suffix));
        if !managed_material_attempt_admitted(
            self.format_version != 0,
            self.phase == ManagedCredentialMutationPhase::Writing,
            writes_new_material,
            self.has_valid_writer_owner(),
            new_refs_bound_to_attempt,
        ) {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }

        let operation = match self.operation {
            ManagedCredentialOperation::Create => 0,
            ManagedCredentialOperation::Update => 1,
            ManagedCredentialOperation::Archive => 2,
            ManagedCredentialOperation::Delete => 3,
        };
        let operation_shape = managed_mutation_operation_shape_allowed(
            operation,
            self.before_credential
                .as_ref()
                .is_some_and(|before| before.lifecycle.is_deleted()),
            self.after_source.status == CredentialStatus::Active,
            self.after_source.status == CredentialStatus::Archived,
            self.after_credential.lifecycle.is_active(),
            matches!(
                self.after_credential.lifecycle,
                ManagedCredentialLifecycle::Archived { .. }
            ),
            self.after_credential.lifecycle.is_deleted(),
        );
        if !operation_shape {
            return Err(ManagedCredentialMutationError::InvalidLifecycle);
        }

        if self.operation == ManagedCredentialOperation::Create {
            if self.before_source.is_some()
                || self.before_credential.is_some()
                || self.after_source.version != 1
                || self.after_credential.revision != 1
            {
                return Err(ManagedCredentialMutationError::RevisionConflict);
            }
            return Ok(());
        }

        let (Some(before_source), Some(before_credential)) =
            (&self.before_source, &self.before_credential)
        else {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        };
        let source_revision_valid = self.after_source.version == before_source.version
            || before_source
                .version
                .checked_add(1)
                .is_some_and(|next| self.after_source.version == next);
        let material_revision_valid = !writes_new_material
            || before_source
                .version
                .checked_add(1)
                .is_some_and(|next| self.after_source.version == next);
        let child_revision_valid = before_credential
            .revision
            .checked_add(1)
            .is_some_and(|next| self.after_credential.revision == next);
        let immutable_source_axes_match = before_source.id == self.after_source.id
            && before_source.workspace_id == self.after_source.workspace_id
            && before_source.kind == self.after_source.kind
            && before_source.descriptor == self.after_source.descriptor
            && before_source.provider_id == self.after_source.provider_id
            && before_source.protocol_endpoint_id == self.after_source.protocol_endpoint_id
            && before_source.env_key == self.after_source.env_key
            && before_source.oauth_command == self.after_source.oauth_command
            && before_source.worker_local_binding == self.after_source.worker_local_binding;
        let child_identity_matches = before_credential.id == self.after_credential.id
            && before_credential.workspace_id == self.after_credential.workspace_id
            && before_credential.vault_id == self.after_credential.vault_id
            && before_credential.source_id == self.after_credential.source_id;
        let before_state_matches_operation = match self.operation {
            ManagedCredentialOperation::Create => false,
            ManagedCredentialOperation::Update => {
                before_source.status == CredentialStatus::Active
                    && before_credential.lifecycle.is_active()
            }
            ManagedCredentialOperation::Archive => before_credential.lifecycle.is_active(),
            ManagedCredentialOperation::Delete => true,
        };
        if !source_revision_valid
            || !material_revision_valid
            || !child_revision_valid
            || !immutable_source_axes_match
            || !child_identity_matches
            || !before_state_matches_operation
        {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        Ok(())
    }

    pub fn create(
        mut source: CredentialSource,
        credential: ManagedVaultCredential,
    ) -> Result<Self, CredentialError> {
        if !managed_creation_pair_admitted(
            source.workspace_id == credential.workspace_id,
            source.id == credential.source_id,
            !credential.vault_id.trim().is_empty(),
            !credential.id.trim().is_empty(),
            source.status == CredentialStatus::Active,
            source.version == 1 && credential.revision == 1 && credential.lifecycle.is_active(),
        ) {
            return Err(CredentialError::InvalidSource(
                "Managed credential creation must freeze one exact active source/child pair".into(),
            ));
        }
        let (writer_token, writer_epoch, writer_lease_expires_at_unix_ms) =
            Self::fresh_writer_lease()?;
        namespace_new_managed_material_refs(None, &mut source, &writer_token);
        Ok(Self {
            format_version: MANAGED_CREDENTIAL_MUTATION_FORMAT_VERSION,
            operation_id: format!("managed-create:{}", source.id.0),
            operation: ManagedCredentialOperation::Create,
            before_source: None,
            after_source: source,
            before_credential: None,
            after_credential: credential,
            phase: ManagedCredentialMutationPhase::Writing,
            attempt_id: writer_token.clone(),
            writer_token,
            writer_epoch,
            writer_lease_expires_at_unix_ms,
        })
    }

    pub fn change(
        operation_id: String,
        operation: ManagedCredentialOperation,
        before_source: CredentialSource,
        mut after_source: CredentialSource,
        before_credential: ManagedVaultCredential,
        after_credential: ManagedVaultCredential,
        writes_new_material: bool,
    ) -> Result<Self, ManagedCredentialMutationError> {
        if operation == ManagedCredentialOperation::Create
            || before_source.id != after_source.id
            || before_source.workspace_id != after_source.workspace_id
            || before_credential.source_id != before_source.id
            || after_credential.source_id != after_source.id
            || before_credential.workspace_id != after_credential.workspace_id
            || before_credential.vault_id != after_credential.vault_id
            || before_source.workspace_id != before_credential.workspace_id
        {
            return Err(ManagedCredentialMutationError::NotFound);
        }
        admit_managed_credential_replacement(
            &before_credential.workspace_id,
            Some(&before_credential),
            before_credential.revision,
            &after_credential,
        )?;
        let (writer_token, writer_epoch, writer_lease_expires_at_unix_ms) = if writes_new_material {
            Self::fresh_writer_lease().map_err(ManagedCredentialMutationError::Store)?
        } else {
            (String::new(), 0, 0)
        };
        if writes_new_material {
            namespace_new_managed_material_refs(
                Some(&before_source),
                &mut after_source,
                &writer_token,
            );
        }
        Ok(Self {
            format_version: MANAGED_CREDENTIAL_MUTATION_FORMAT_VERSION,
            operation_id,
            operation,
            before_source: Some(before_source),
            after_source,
            before_credential: Some(before_credential),
            after_credential,
            phase: if writes_new_material {
                ManagedCredentialMutationPhase::Writing
            } else {
                ManagedCredentialMutationPhase::Ready
            },
            attempt_id: writer_token.clone(),
            writer_token,
            writer_epoch,
            writer_lease_expires_at_unix_ms,
        })
    }

    #[must_use]
    pub fn source_id(&self) -> &CredentialSourceId {
        &self.after_source.id
    }
}

fn namespace_new_managed_material_refs(
    before: Option<&CredentialSource>,
    after: &mut CredentialSource,
    attempt_id: &str,
) {
    let before_refs = before
        .into_iter()
        .flat_map(CredentialSource::material_refs)
        .cloned()
        .collect::<HashSet<_>>();
    let suffix = format!(":attempt:{attempt_id}");
    if let Some(reference) = after.material_ref.as_mut()
        && !before_refs.contains(reference)
        && !reference.0.ends_with(&suffix)
    {
        reference.0.push_str(&suffix);
    }
    for reference in after.auxiliary_material_refs.values_mut() {
        if !before_refs.contains(reference) && !reference.0.ends_with(&suffix) {
            reference.0.push_str(&suffix);
        }
    }
}

fn managed_credential_now_unix_ms() -> Result<u64, CredentialError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CredentialError::Storage(format!("credential clock: {error}")))?
        .as_millis();
    u64::try_from(millis)
        .map_err(|_| CredentialError::Storage("credential clock overflowed u64".into()))
}

#[cfg(any(test, feature = "test-support"))]
fn invalid_pending_mutation(error: ManagedCredentialMutationError) -> CredentialError {
    match error {
        ManagedCredentialMutationError::Store(error) => error,
        error => CredentialError::MutationConflict(format!(
            "invalid durable Managed credential mutation: {error}"
        )),
    }
}

#[cfg(kani)]
#[kani::proof]
fn managed_creation_pair_requires_every_binding_axis() {
    let axes = [
        kani::any::<bool>(),
        kani::any::<bool>(),
        kani::any::<bool>(),
        kani::any::<bool>(),
        kani::any::<bool>(),
        kani::any::<bool>(),
    ];
    let admitted =
        managed_creation_pair_admitted(axes[0], axes[1], axes[2], axes[3], axes[4], axes[5]);
    assert_eq!(admitted, axes.into_iter().all(|axis| axis));
    if admitted {
        assert!(axes.into_iter().all(|axis| axis));
    }
}

#[cfg(kani)]
#[kani::proof]
fn managed_creation_begin_rejects_every_published_identity() {
    let source_published = kani::any::<bool>();
    let child_published = kani::any::<bool>();
    let admitted = managed_creation_begin_allowed(source_published, child_published);
    assert_eq!(admitted, !(source_published || child_published));
    if admitted {
        assert!(!source_published && !child_published);
    }
}

#[cfg(kani)]
#[kani::proof]
fn managed_mutation_operation_shape_is_closed_and_delete_is_absorbing() {
    let operation = kani::any::<u8>();
    let before_deleted = kani::any::<bool>();
    let source_active = kani::any::<bool>();
    let source_archived = kani::any::<bool>();
    let child_active = kani::any::<bool>();
    let child_archived = kani::any::<bool>();
    let child_deleted = kani::any::<bool>();
    let admitted = managed_mutation_operation_shape_allowed(
        operation,
        before_deleted,
        source_active,
        source_archived,
        child_active,
        child_archived,
        child_deleted,
    );
    if admitted {
        assert!(operation <= 3);
        assert!(!before_deleted || child_deleted);
        match operation {
            0 | 1 => assert!(source_active && child_active),
            2 => assert!(source_archived && child_archived),
            3 => assert!(source_archived && child_deleted),
            _ => unreachable!(),
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn managed_material_attempt_requires_owner_and_exact_physical_namespace() {
    let writing = kani::any::<bool>();
    let writes_new_material = kani::any::<bool>();
    let valid_owner = kani::any::<bool>();
    let new_refs_bound_to_attempt = kani::any::<bool>();
    let admitted = managed_material_attempt_admitted(
        true,
        writing,
        writes_new_material,
        valid_owner,
        new_refs_bound_to_attempt,
    );
    if admitted && (writing || writes_new_material) {
        assert!(valid_owner);
    }
    if admitted && writes_new_material {
        assert!(new_refs_bound_to_attempt);
    }
}

/// One physical persistence boundary for every Managed Credential command.
/// Implementations publish Source + child/tombstone and advance the durable
/// fact to `Reclaiming` in one local transaction.
#[async_trait::async_trait]
pub trait ManagedCredentialRepository: CredentialRepo + ManagedVaultRepo {
    async fn begin_managed_mutation(
        &self,
        pending: PendingManagedCredentialMutation,
    ) -> Result<(), CredentialError>;
    async fn commit_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, ManagedCredentialMutationError>;
    async fn mark_managed_mutation_ready(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError>;
    async fn pending_managed_mutations(
        &self,
    ) -> Result<Vec<PendingManagedCredentialMutation>, CredentialError>;
    /// Atomically fence one exact expired `Writing` owner. `None` means the
    /// lease is still live, the phase is no longer `Writing`, or another worker
    /// already changed the durable snapshot.
    async fn claim_expired_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<PendingManagedCredentialMutation>, CredentialError>;
    /// Atomically retain the exact unpublished mutation as a durable cleanup
    /// authority. Implementations must compare both the complete pending fact
    /// and its before-pair truth, then transition it to `ReclaimingAbort`.
    /// Absence or any stale owner/snapshot is a conflict, never an idempotent
    /// success: callers may delete external material only from the returned
    /// durable fact.
    async fn abort_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError>;
    async fn complete_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<(), CredentialError>;
    /// Read one exact durable rollout through its primary identity. Request
    /// paths use this bounded lookup; only the supervised reconciler enumerates
    /// the full outbox.
    async fn managed_rollout(
        &self,
        event_id: &str,
    ) -> Result<Option<ManagedCredentialRollout>, CredentialError>;
    async fn pending_managed_rollouts(
        &self,
    ) -> Result<Vec<ManagedCredentialRollout>, CredentialError>;
    async fn complete_managed_rollout(
        &self,
        rollout: &ManagedCredentialRollout,
    ) -> Result<(), CredentialError>;
    /// Vault roots carrying a durable delete request. Implementations return
    /// every workspace because this method is consumed only by the supervised
    /// domain reconciler, never by an authorization surface.
    async fn pending_managed_vault_deletions(&self) -> Result<Vec<ManagedVault>, CredentialError>;
}

/// Service-owned rolling replacement edge. Credential never knows whether the
/// consumer is a Session, Deployment, Kubernetes controller, or remote service;
/// it only requires an idempotent adoption of the exact committed fence.
#[async_trait::async_trait]
pub trait ManagedCredentialRolloutTarget: Send + Sync {
    async fn rollout(
        &self,
        event: &ManagedCredentialRollout,
    ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError>;
}

/// Deliver all currently durable rollout events. A failed target leaves the
/// event pending for the next supervisor tick; successful acknowledgement is
/// exact-event guarded so a stale worker cannot remove a newer notification.
pub async fn reconcile_managed_credential_rollouts(
    repo: &dyn ManagedCredentialRepository,
    target: &dyn ManagedCredentialRolloutTarget,
) -> Result<usize, CredentialError> {
    let mut completed = 0;
    let mut first_error = None;
    for event in repo.pending_managed_rollouts().await? {
        match reconcile_managed_credential_rollout(&event, repo, target).await {
            Ok(progress) if progress.is_converged() => completed += 1,
            Ok(_) => {}
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    first_error.map_or(Ok(completed), Err)
}

/// Deliver and, only after convergence, exactly acknowledge one durable
/// rollout event. Target failures and an explicitly pending target both leave
/// the event durable and return [`ManagedCredentialAdoptionProgress::Pending`].
pub async fn reconcile_managed_credential_rollout(
    event: &ManagedCredentialRollout,
    repo: &dyn ManagedCredentialRepository,
    target: &dyn ManagedCredentialRolloutTarget,
) -> Result<ManagedCredentialAdoptionProgress, CredentialError> {
    let Ok(progress) = target.rollout(event).await else {
        return Ok(ManagedCredentialAdoptionProgress::Pending);
    };
    if progress.is_converged() {
        repo.complete_managed_rollout(event).await?;
    }
    Ok(progress)
}

/// Resume one durable Vault-root deletion. Child retirement reuses the exact
/// Managed Credential transaction and rollout protocol; root completion waits
/// until every child is a tombstone and no rollout for this Vault remains.
pub async fn reconcile_managed_vault_deletion(
    vault: &ManagedVault,
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<bool, ManagedCredentialMutationError> {
    if vault.is_deleted() {
        return Ok(true);
    }
    if !vault.deletion_requested() {
        return Err(ManagedCredentialMutationError::InvalidLifecycle);
    }
    let operation_id = vault
        .deletion
        .as_ref()
        .expect("requested deletion was checked")
        .operation_id
        .clone();
    for child in repo
        .list_vault_credentials(&vault.workspace_id, &vault.id)
        .await?
    {
        if !child.lifecycle.is_deleted() {
            retire_managed_credential(
                &vault.workspace_id,
                &vault.id,
                &child.id,
                ManagedCredentialOperation::Delete,
                vault
                    .deletion
                    .as_ref()
                    .expect("requested deletion was checked")
                    .requested_at
                    .clone(),
                store,
                repo,
            )
            .await?;
        }
    }

    let has_unsettled_rollout = repo
        .pending_managed_rollouts()
        .await?
        .into_iter()
        .any(|rollout| rollout.workspace_id == vault.workspace_id && rollout.vault_id == vault.id);
    if has_unsettled_rollout {
        return Ok(false);
    }

    let current = repo
        .get_vault(&vault.workspace_id, &vault.id)
        .await?
        .filter(|current| {
            current
                .deletion
                .as_ref()
                .is_some_and(|deletion| deletion.operation_id == operation_id)
        })
        .ok_or(ManagedCredentialMutationError::NotFound)?;
    if current.is_deleted() {
        return Ok(true);
    }
    let (completed, changed) =
        complete_managed_vault_deletion(&current).map_err(|error| match error {
            crate::catalog::ManagedVaultMutationError::NotFound => {
                ManagedCredentialMutationError::NotFound
            }
            crate::catalog::ManagedVaultMutationError::RevisionConflict => {
                ManagedCredentialMutationError::RevisionConflict
            }
            crate::catalog::ManagedVaultMutationError::RevisionExhausted => {
                ManagedCredentialMutationError::RevisionExhausted
            }
            crate::catalog::ManagedVaultMutationError::InvalidLifecycle => {
                ManagedCredentialMutationError::InvalidLifecycle
            }
            crate::catalog::ManagedVaultMutationError::Store(error) => {
                ManagedCredentialMutationError::Store(error)
            }
        })?;
    if changed {
        repo.replace_vault(&current.workspace_id, current.revision, completed)
            .await
            .map_err(|error| match error {
                crate::catalog::ManagedVaultMutationError::NotFound => {
                    ManagedCredentialMutationError::NotFound
                }
                crate::catalog::ManagedVaultMutationError::RevisionConflict => {
                    ManagedCredentialMutationError::RevisionConflict
                }
                crate::catalog::ManagedVaultMutationError::RevisionExhausted => {
                    ManagedCredentialMutationError::RevisionExhausted
                }
                crate::catalog::ManagedVaultMutationError::InvalidLifecycle => {
                    ManagedCredentialMutationError::InvalidLifecycle
                }
                crate::catalog::ManagedVaultMutationError::Store(error) => {
                    ManagedCredentialMutationError::Store(error)
                }
            })?;
    }
    Ok(true)
}

/// Resume every durable root deletion after process restart or request
/// cancellation. A blocked Vault remains requested for the next pass.
pub async fn reconcile_managed_vault_deletions(
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<usize, ManagedCredentialMutationError> {
    let pending = repo.pending_managed_vault_deletions().await?;
    let mut completed = 0;
    let mut first_error = None;
    for vault in pending {
        match reconcile_managed_vault_deletion(&vault, store, repo).await {
            Ok(true) => completed += 1,
            Ok(false) => {}
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    first_error.map_or(Ok(completed), Err)
}

#[cfg(kani)]
#[kani::proof]
fn managed_rollout_never_precedes_exact_pair_publication() {
    let pair_committed = kani::any::<bool>();
    let is_create = kani::any::<bool>();
    let exact_source_fence = kani::any::<bool>();
    let exact_child_fence = kani::any::<bool>();
    let published = managed_rollout_publish_allowed(
        pair_committed,
        is_create,
        exact_source_fence,
        exact_child_fence,
    );
    if published {
        assert!(pair_committed);
        assert!(!is_create);
        assert!(exact_source_fence && exact_child_fence);
    }
}

/// Write-only Managed creation command. Plaintext is consumed by the
/// application service and never enters the durable pending fact.
pub struct ManagedCredentialCreateCommand {
    pub source: CredentialCreateParams,
    /// Stable identity for an idempotent domain command. Ordinary Managed API
    /// creates leave this absent and receive the existing generated identity.
    pub source_id: Option<CredentialSourceId>,
    pub protocol_endpoint_id: Option<String>,
    pub primary_material_ref: Option<crate::SecretRef>,
    pub auxiliary_materials: BTreeMap<String, awaken_agent_contract::RedactedString>,
    pub credential_id: String,
    pub vault_id: String,
    pub auth: crate::catalog::ManagedCredentialAuth,
    pub metadata: BTreeMap<String, String>,
    pub display_name: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedCredentialCreationError {
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Admission(#[from] ManagedCredentialAdmissionError),
}

impl From<ManagedCredentialMutationError> for ManagedCredentialCreationError {
    fn from(error: ManagedCredentialMutationError) -> Self {
        match error {
            ManagedCredentialMutationError::Admission(error) => Self::Admission(error),
            ManagedCredentialMutationError::Store(error) => Self::Credential(error),
            error => Self::Credential(CredentialError::MutationConflict(error.to_string())),
        }
    }
}

/// Terminal material-reclaim outcome. Both variants make the source
/// non-materializable; `Archived` additionally communicates terminal retention
/// to management projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialRetirement {
    Disable,
    Archive,
}

/// Material changes published as one credential revision. `primary = None`
/// preserves the compatibility primary slot; auxiliary `Some` values rotate a
/// named slot and `None` values remove it. Slot names are extension-owned.
#[derive(Default)]
pub struct CredentialMaterialPatch {
    pub primary: Option<awaken_agent_contract::RedactedString>,
    pub auxiliary: BTreeMap<String, Option<awaken_agent_contract::RedactedString>>,
    /// Optional replacement metadata published by the same exact-revision CAS.
    /// Omission preserves the current descriptor, including for legacy rows.
    pub descriptor: Option<awaken_credential_contract::CredentialDescriptor>,
}

/// The credential-source store port. Secret-free rows only. Pools are stored here
/// too (they are secret-free groupings of sources the resolver fails over across).
#[async_trait::async_trait]
pub trait CredentialRepo: Send + Sync {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError>;
    /// Atomically retain an existing source with the same id or insert `source`.
    /// The returned row is the durable winner.
    async fn put_if_absent(
        &self,
        source: CredentialSource,
    ) -> Result<CredentialSource, CredentialError>;
    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError>;
    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError>;

    /// Claim this source-keyed WAL intent. `true` means this caller inserted
    /// the intent; `false` means an identical durable intent already existed.
    async fn begin_mutation(
        &self,
        intent: CredentialMutationIntent,
    ) -> Result<bool, CredentialError>;
    /// Atomically compare/publish the source while retaining the WAL intent until
    /// material cleanup completes.
    async fn apply_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError>;
    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError>;
    async fn complete_mutation(&self, id: &CredentialSourceId) -> Result<(), CredentialError>;
    /// Every material reference reachable from committed metadata.
    async fn material_refs(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        Err(CredentialError::Storage(
            "credential material inventory is not supported by this repository".to_string(),
        ))
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError>;
    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError>;
    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError>;
}

/// In-memory [`CredentialRepo`] for tests and scenario fixtures.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub(super) struct RepoState {
    rows: HashMap<String, CredentialSource>,
    pools: HashMap<String, CredentialPool>,
    intents: HashMap<String, CredentialMutationIntent>,
    managed_mutations: HashMap<String, PendingManagedCredentialMutation>,
    managed_rollouts: HashMap<String, ManagedCredentialRollout>,
    pub(super) vaults: HashMap<String, ManagedVault>,
    pub(super) vault_credentials: HashMap<String, ManagedVaultCredential>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct InMemoryCredentialRepo {
    pub(super) state: Mutex<RepoState>,
}

#[cfg(any(test, feature = "test-support"))]
impl InMemoryCredentialRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
impl CredentialRepo for InMemoryCredentialRepo {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .rows
            .insert(source.id.0.clone(), source);
        Ok(())
    }

    async fn put_if_absent(
        &self,
        source: CredentialSource,
    ) -> Result<CredentialSource, CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        Ok(state
            .rows
            .entry(source.id.0.clone())
            .or_insert(source)
            .clone())
    }

    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .rows
            .get(&id.0)
            .cloned()
            .ok_or_else(|| CredentialError::SourceNotFound(id.0.clone()))
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .rows
            .values()
            .filter(|s| s.workspace_id == workspace_id)
            .cloned()
            .collect())
    }

    async fn begin_mutation(
        &self,
        intent: CredentialMutationIntent,
    ) -> Result<bool, CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        match state.intents.entry(intent.after.id.0.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(intent);
                Ok(true)
            }
            std::collections::hash_map::Entry::Occupied(entry) if entry.get() == &intent => {
                Ok(false)
            }
            std::collections::hash_map::Entry::Occupied(_) => Err(
                CredentialError::MutationConflict("another credential mutation is pending".into()),
            ),
        }
    }

    async fn apply_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        if state.intents.get(&intent.after.id.0) != Some(intent) {
            return Err(CredentialError::MutationConflict(
                "credential mutation has no matching durable intent".into(),
            ));
        }
        let current = state.rows.get(&intent.after.id.0);
        if current == Some(&intent.after) {
            return Ok(());
        }
        if current != intent.before.as_ref() {
            return Err(CredentialError::MutationConflict(
                "credential revision changed during mutation".into(),
            ));
        }
        state
            .rows
            .insert(intent.after.id.0.clone(), intent.after.clone());
        Ok(())
    }

    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .intents
            .values()
            .cloned()
            .collect())
    }

    async fn complete_mutation(&self, id: &CredentialSourceId) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .intents
            .remove(&id.0);
        Ok(())
    }

    async fn material_refs(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .rows
            .values()
            .flat_map(|source| source.material_refs().cloned())
            .collect())
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .pools
            .insert(pool.id.0.clone(), pool);
        Ok(())
    }

    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .pools
            .get(&id.0)
            .cloned()
            .ok_or_else(|| CredentialError::PoolNotFound(id.0.clone()))
    }

    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .pools
            .values()
            .filter(|p| p.workspace_id == workspace_id)
            .cloned()
            .collect())
    }
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
impl ManagedCredentialRepository for InMemoryCredentialRepo {
    async fn begin_managed_mutation(
        &self,
        pending: PendingManagedCredentialMutation,
    ) -> Result<(), CredentialError> {
        pending
            .validate_for_begin()
            .map_err(invalid_pending_mutation)?;
        let mut state = self.state.lock().expect("credential repo");
        let current_source = state.rows.get(&pending.after_source.id.0);
        let current_child = state.vault_credentials.get(&pending.after_credential.id);
        if current_source != pending.before_source.as_ref()
            || current_child != pending.before_credential.as_ref()
        {
            return Err(CredentialError::MutationConflict(
                "Managed credential changed before its mutation was prepared".into(),
            ));
        }
        match state
            .managed_mutations
            .entry(pending.after_source.id.0.clone())
        {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(pending);
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(entry) if entry.get() == &pending => Ok(()),
            std::collections::hash_map::Entry::Occupied(_) => {
                Err(CredentialError::MutationConflict(
                    "another Managed credential mutation is pending".into(),
                ))
            }
        }
    }

    async fn commit_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, ManagedCredentialMutationError> {
        pending.validate()?;
        let mut state = self.state.lock().expect("credential repo");
        let durable = state
            .managed_mutations
            .get(&pending.after_source.id.0)
            .cloned()
            .ok_or_else(|| {
                ManagedCredentialMutationError::Store(CredentialError::MutationConflict(
                    "Managed credential mutation has no durable pending fact".into(),
                ))
            })?;
        if durable.phase == ManagedCredentialMutationPhase::Reclaiming {
            if durable.operation_id == pending.operation_id
                && durable.writer_token == pending.writer_token
                && durable.writer_epoch == pending.writer_epoch
                && state.rows.get(&pending.after_source.id.0) == Some(&pending.after_source)
                && state.vault_credentials.get(&pending.after_credential.id)
                    == Some(&pending.after_credential)
            {
                return Ok(durable);
            }
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        if &durable != pending || pending.phase != ManagedCredentialMutationPhase::Ready {
            return Err(ManagedCredentialMutationError::Store(
                CredentialError::MutationConflict(
                    "Managed credential mutation is not ready or does not match its durable fact"
                        .into(),
                ),
            ));
        }

        let current_source = state.rows.get(&pending.after_source.id.0);
        let current_child = state.vault_credentials.get(&pending.after_credential.id);
        if current_source != pending.before_source.as_ref()
            || current_child != pending.before_credential.as_ref()
        {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }

        let vault = state.vaults.get(&pending.after_credential.vault_id);
        let existing = state
            .vault_credentials
            .values()
            .filter(|current| {
                current.workspace_id == pending.after_credential.workspace_id
                    && current.vault_id == pending.after_credential.vault_id
                    && current.id != pending.after_credential.id
                    && !current.lifecycle.is_deleted()
            })
            .cloned()
            .collect::<Vec<_>>();
        match pending.operation {
            ManagedCredentialOperation::Create => {
                if state
                    .vault_credentials
                    .values()
                    .any(|credential| credential.source_id == pending.after_source.id)
                {
                    return Err(ManagedCredentialMutationError::RevisionConflict);
                }
                admit_managed_credential_insert(
                    &pending.after_credential.workspace_id,
                    vault,
                    &existing,
                    &pending.after_credential,
                )?;
            }
            ManagedCredentialOperation::Update => {
                admit_managed_credential_replacement(
                    &pending.after_credential.workspace_id,
                    pending.before_credential.as_ref(),
                    pending
                        .before_credential
                        .as_ref()
                        .ok_or(ManagedCredentialMutationError::NotFound)?
                        .revision,
                    &pending.after_credential,
                )?;
                admit_managed_credential_insert(
                    &pending.after_credential.workspace_id,
                    vault,
                    &existing,
                    &ManagedVaultCredential {
                        revision: 1,
                        lifecycle: ManagedCredentialLifecycle::Active,
                        ..pending.after_credential.clone()
                    },
                )?;
            }
            ManagedCredentialOperation::Archive | ManagedCredentialOperation::Delete => {
                admit_managed_credential_replacement(
                    &pending.after_credential.workspace_id,
                    pending.before_credential.as_ref(),
                    pending
                        .before_credential
                        .as_ref()
                        .ok_or(ManagedCredentialMutationError::NotFound)?
                        .revision,
                    &pending.after_credential,
                )?;
                if !managed_retirement_parent_admitted(
                    pending.operation,
                    vault,
                    &pending.after_credential,
                ) {
                    return Err(if vault.is_some() {
                        ManagedCredentialMutationError::InvalidLifecycle
                    } else {
                        ManagedCredentialMutationError::NotFound
                    });
                }
            }
        }
        let rollout = managed_rollout_from_committed(pending);
        if rollout.as_ref().is_some_and(|proposed| {
            state
                .managed_rollouts
                .get(&proposed.id)
                .is_some_and(|durable| durable != proposed)
        }) {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        state.rows.insert(
            pending.after_source.id.0.clone(),
            pending.after_source.clone(),
        );
        state.vault_credentials.insert(
            pending.after_credential.id.clone(),
            pending.after_credential.clone(),
        );
        if let Some(rollout) = rollout {
            state
                .managed_rollouts
                .entry(rollout.id.clone())
                .or_insert(rollout);
        }
        let mut reclaiming = pending.clone();
        reclaiming.phase = ManagedCredentialMutationPhase::Reclaiming;
        state
            .managed_mutations
            .insert(pending.after_source.id.0.clone(), reclaiming.clone());
        Ok(reclaiming)
    }

    async fn mark_managed_mutation_ready(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError> {
        pending.validate().map_err(invalid_pending_mutation)?;
        let mut state = self.state.lock().expect("credential repo");
        let durable = state
            .managed_mutations
            .get_mut(&pending.after_source.id.0)
            .ok_or_else(|| {
                CredentialError::MutationConflict(
                    "Managed credential mutation has no durable pending fact".into(),
                )
            })?;
        if durable != pending || pending.phase != ManagedCredentialMutationPhase::Writing {
            return Err(CredentialError::MutationConflict(
                "Managed credential ready transition does not match Writing".into(),
            ));
        }
        durable.phase = ManagedCredentialMutationPhase::Ready;
        Ok(durable.clone())
    }

    async fn pending_managed_mutations(
        &self,
    ) -> Result<Vec<PendingManagedCredentialMutation>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .managed_mutations
            .values()
            .cloned()
            .collect())
    }

    async fn claim_expired_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<PendingManagedCredentialMutation>, CredentialError> {
        let Some(claimed) = pending.claim_after_expiry(now_unix_ms, lease_expires_at_unix_ms)?
        else {
            return Ok(None);
        };
        claimed.validate().map_err(invalid_pending_mutation)?;
        let mut state = self.state.lock().expect("credential repo");
        let Some(durable) = state.managed_mutations.get_mut(&pending.after_source.id.0) else {
            return Ok(None);
        };
        if durable != pending || durable.phase != ManagedCredentialMutationPhase::Writing {
            return Ok(None);
        }
        *durable = claimed.clone();
        Ok(Some(claimed))
    }

    async fn abort_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError> {
        pending.validate().map_err(invalid_pending_mutation)?;
        let mut state = self.state.lock().expect("credential repo");
        if state.rows.get(&pending.after_source.id.0) != pending.before_source.as_ref()
            || state.vault_credentials.get(&pending.after_credential.id)
                != pending.before_credential.as_ref()
        {
            return Err(CredentialError::MutationConflict(
                "cannot abort a published or superseded Managed credential mutation".into(),
            ));
        }
        let durable = state
            .managed_mutations
            .get_mut(&pending.after_source.id.0)
            .ok_or_else(|| {
                CredentialError::MutationConflict(
                    "Managed credential abort has no durable pending fact".into(),
                )
            })?;
        if durable == pending && pending.phase == ManagedCredentialMutationPhase::ReclaimingAbort {
            return Ok(durable.clone());
        }
        if durable != pending
            || !matches!(
                pending.phase,
                ManagedCredentialMutationPhase::Writing | ManagedCredentialMutationPhase::Ready
            )
        {
            return Err(CredentialError::MutationConflict(
                "Managed credential abort does not match its durable pending fact".into(),
            ));
        }
        durable.phase = ManagedCredentialMutationPhase::ReclaimingAbort;
        Ok(durable.clone())
    }

    async fn complete_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<(), CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        let durable = state.managed_mutations.get(&pending.after_source.id.0);
        if durable.is_none() {
            return Ok(());
        }
        let truth_matches_phase = match pending.phase {
            ManagedCredentialMutationPhase::Reclaiming => {
                state.rows.get(&pending.after_source.id.0) == Some(&pending.after_source)
                    && state.vault_credentials.get(&pending.after_credential.id)
                        == Some(&pending.after_credential)
            }
            ManagedCredentialMutationPhase::ReclaimingAbort => {
                state.rows.get(&pending.after_source.id.0) == pending.before_source.as_ref()
                    && state.vault_credentials.get(&pending.after_credential.id)
                        == pending.before_credential.as_ref()
            }
            ManagedCredentialMutationPhase::Writing | ManagedCredentialMutationPhase::Ready => {
                false
            }
        };
        if durable != Some(pending) || !truth_matches_phase {
            return Err(CredentialError::MutationConflict(
                "Managed credential completion does not match its durable cleanup truth".into(),
            ));
        }
        state.managed_mutations.remove(&pending.after_source.id.0);
        Ok(())
    }

    async fn pending_managed_rollouts(
        &self,
    ) -> Result<Vec<ManagedCredentialRollout>, CredentialError> {
        let mut events = self
            .state
            .lock()
            .expect("credential repo")
            .managed_rollouts
            .values()
            .cloned()
            .collect::<Vec<_>>();
        events.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(events)
    }

    async fn managed_rollout(
        &self,
        event_id: &str,
    ) -> Result<Option<ManagedCredentialRollout>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .managed_rollouts
            .get(event_id)
            .cloned())
    }

    async fn complete_managed_rollout(
        &self,
        rollout: &ManagedCredentialRollout,
    ) -> Result<(), CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        match state.managed_rollouts.get(&rollout.id) {
            Some(durable) if durable == rollout => {
                state.managed_rollouts.remove(&rollout.id);
                Ok(())
            }
            None => Ok(()),
            Some(_) => Err(CredentialError::MutationConflict(
                "Managed credential rollout acknowledgement is stale".into(),
            )),
        }
    }

    async fn pending_managed_vault_deletions(&self) -> Result<Vec<ManagedVault>, CredentialError> {
        let mut vaults = self
            .state
            .lock()
            .expect("credential repo")
            .vaults
            .values()
            .filter(|vault| vault.deletion_requested())
            .cloned()
            .collect::<Vec<_>>();
        vaults.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(vaults)
    }
}

/// Idempotently register one Worker-owned, non-secret local credential binding.
/// Primary-key identity is a collision-free length-prefixed projection of the
/// tuple, making the existing repository constraint the uniqueness authority.
mod credential_operations;
mod inventory;

pub use credential_operations::*;
pub use inventory::*;
