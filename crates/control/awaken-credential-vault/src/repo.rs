//! The credential source repository port (ADR-0043) — stores the **secret-free**
//! [`CredentialSource`] rows (the sealed material lives behind [`SecretStore`], a
//! separate port). Its own `credential` migration scope is what lets the whole
//! domain be split into its own database/service (blast-radius isolation).

#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;
use std::collections::{BTreeMap, HashSet};
#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;

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
mod material_mutation;
pub use material_mutation::{
    CREDENTIAL_MATERIAL_WRITER_LEASE_MS, CredentialMaterialMutationFence,
    CredentialMaterialMutationPhase,
};
use material_mutation::{
    CredentialMaterialRecoveryAction, credential_material_now_unix_ms,
    credential_material_recovery_action, fence_material_writes,
    idempotent_credential_source_matches, logical_credential_source_matches,
    namespace_new_material_refs,
};

/// Secret-free write-ahead intent for create, rotate, disable, archive, or
/// revoke. `before = None` is creation. A `before` with the same id is an exact
/// revision mutation. A `before` with a different id is a distinct replacement
/// create: publication compares the old source exactly, inserts `after`, and
/// deliberately leaves the old source unchanged for its pinned consumers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CredentialMutationIntent {
    #[serde(flatten)]
    pub material_fence: CredentialMaterialMutationFence,
    #[serde(default)]
    pub before: Option<CredentialSource>,
    #[serde(alias = "source")]
    pub after: CredentialSource,
}

impl CredentialMutationIntent {
    /// Validate the physical repository key before a decoded recovery envelope
    /// can reach the external SecretStore participant. The JSON payload owns
    /// the command identity; the table/map key may only index that exact id.
    pub fn validate_durable_key(&self, source_id: &str) -> Result<(), CredentialError> {
        validate_mutation_durable_key(source_id, &self.after.id)
    }

    /// Prepare one source-only publication envelope with the same leased,
    /// attempt-specific external-effect fence used by Managed pair mutations.
    pub fn prepare(
        before: Option<CredentialSource>,
        mut after: CredentialSource,
    ) -> Result<Self, CredentialError> {
        let before_refs = before
            .iter()
            .flat_map(CredentialSource::material_refs)
            .collect::<HashSet<_>>();
        let writes_new_material = after
            .material_refs()
            .any(|reference| !before_refs.contains(reference));
        let material_fence = CredentialMaterialMutationFence::fresh(writes_new_material)?;
        namespace_new_material_refs(before.as_ref(), &mut after, material_fence.attempt_id());
        let intent = Self {
            material_fence,
            before,
            after,
        };
        intent.validate_for_begin()?;
        Ok(intent)
    }

    /// Whether this intent creates a distinct replacement instead of changing
    /// one source in place.
    #[must_use]
    pub fn is_distinct_replacement(&self) -> bool {
        self.before
            .as_ref()
            .is_some_and(|before| before.id != self.after.id)
    }

    /// Whether two fresh requests name the same logical command while one
    /// durable pending row already owns its random physical attempt. This never
    /// transfers writer ownership; repositories return `false` from `begin` so
    /// the follower performs zero SecretStore writes and retries after the
    /// owner publishes or recovery aborts it.
    #[must_use]
    pub fn matches_logical_command(&self, candidate: &Self) -> bool {
        self.before == candidate.before
            && logical_credential_source_matches(&self.after, &candidate.after)
    }

    /// Validate the invariant that lets every repository adapter interpret a
    /// cross-id intent identically. The repository owns only publication shape:
    /// one Workspace/kind, an active version-1 insertion, and disjoint material.
    /// A higher application layer owns provider/descriptor replacement policy.
    pub fn validate_for_begin(&self) -> Result<(), CredentialError> {
        self.validate()?;
        let before_refs = self
            .before
            .iter()
            .flat_map(CredentialSource::material_refs)
            .collect::<HashSet<_>>();
        let writes_new_material = self
            .after
            .material_refs()
            .any(|reference| !before_refs.contains(reference));
        let suffix = format!(":attempt:{}", self.material_fence.attempt_id());
        let new_refs_bound_to_attempt = !self.material_fence.attempt_id().is_empty()
            && self
                .after
                .material_refs()
                .filter(|reference| !before_refs.contains(reference))
                .all(|reference| reference.0.ends_with(&suffix));
        self.material_fence
            .validate_fresh_for_begin(writes_new_material, new_refs_bound_to_attempt)
    }

    pub fn validate(&self) -> Result<(), CredentialError> {
        self.after.validate_authority()?;
        let Some(before) = self.before.as_ref() else {
            if self.after.replacement_of.is_some() {
                return Err(CredentialError::InvalidSource(
                    "ordinary credential creation cannot publish replacement provenance".into(),
                ));
            }
            return self.validate_material_fence();
        };
        before.validate_authority()?;
        if before.id == self.after.id {
            if before.replacement_of != self.after.replacement_of {
                return Err(CredentialError::InvalidSource(
                    "credential replacement provenance is immutable across one-source mutations"
                        .into(),
                ));
            }
            return self.validate_material_fence();
        }
        let before_revision = u64::try_from(before.version).map_err(|_| {
            CredentialError::InvalidSource(
                "credential replacement predecessor revision must be positive".into(),
            )
        })?;
        let expected_provenance = awaken_credential_contract::CredentialRef {
            id: before.id.0.clone(),
            revision: before_revision,
        };
        let valid_publication = !before.id.0.trim().is_empty()
            && !self.after.id.0.trim().is_empty()
            && before.workspace_id == self.after.workspace_id
            && before.kind == self.after.kind
            && before.status == CredentialStatus::Active
            && self.after.status == CredentialStatus::Active
            && before.version > 0
            && self.after.version == 1
            && self.after.replacement_of.as_ref() == Some(&expected_provenance);
        let before_refs = before.material_refs().collect::<HashSet<_>>();
        let after_refs = self.after.material_refs().collect::<HashSet<_>>();
        if !valid_publication || !before_refs.is_disjoint(&after_refs) {
            return Err(CredentialError::InvalidSource(
                "a distinct replacement must preserve one active Workspace/kind publication boundary and own disjoint material"
                    .into(),
            ));
        }
        self.validate_material_fence()
    }

    fn validate_material_fence(&self) -> Result<(), CredentialError> {
        let before_refs = self
            .before
            .iter()
            .flat_map(CredentialSource::material_refs)
            .collect::<HashSet<_>>();
        let writes_new_material = self
            .after
            .material_refs()
            .any(|reference| !before_refs.contains(reference));
        let suffix = format!(":attempt:{}", self.material_fence.attempt_id());
        let new_refs_bound_to_attempt = !self.material_fence.attempt_id().is_empty()
            && self
                .after
                .material_refs()
                .filter(|reference| !before_refs.contains(reference))
                .all(|reference| reference.0.ends_with(&suffix));
        self.material_fence
            .validate(writes_new_material, new_refs_bound_to_attempt)
    }

    pub fn claim_after_expiry(
        &self,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<Self>, CredentialError> {
        let Some(material_fence) = self
            .material_fence
            .claim_after_expiry(now_unix_ms, lease_expires_at_unix_ms)?
        else {
            return Ok(None);
        };
        let mut claimed = self.clone();
        claimed.material_fence = material_fence;
        claimed.validate()?;
        Ok(Some(claimed))
    }

    pub fn with_material_ready(&self) -> Result<Self, CredentialError> {
        let mut ready = self.clone();
        ready.material_fence = self.material_fence.ready()?;
        ready.validate()?;
        Ok(ready)
    }

    pub fn with_material_reclaiming(&self) -> Result<Self, CredentialError> {
        let mut reclaiming = self.clone();
        reclaiming.material_fence = self.material_fence.reclaiming()?;
        reclaiming.validate()?;
        Ok(reclaiming)
    }

    pub fn with_material_reclaiming_abort(&self) -> Result<Self, CredentialError> {
        let mut reclaiming = self.clone();
        reclaiming.material_fence = self.material_fence.reclaiming_abort()?;
        reclaiming.validate()?;
        Ok(reclaiming)
    }
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

/// Secret-free durable authority for every Managed Credential mutation.
/// Source and management child are one consistency pair. Plaintext never enters
/// this fact; only frozen references cross the recovery boundary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingManagedCredentialMutation {
    /// Serialization format discriminator. Kept private so ordinary callers
    /// cannot construct a durable authority with a struct literal; repository
    /// adapters deserialize legacy rows as version zero and may only recover
    /// them, never admit them as a new command.
    #[serde(flatten)]
    pub material_fence: CredentialMaterialMutationFence,
    pub operation_id: String,
    pub operation: ManagedCredentialOperation,
    #[serde(default)]
    pub before_source: Option<CredentialSource>,
    pub after_source: CredentialSource,
    #[serde(default)]
    pub before_credential: Option<ManagedVaultCredential>,
    pub after_credential: ManagedVaultCredential,
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
    ordinary_lineage: bool,
) -> bool {
    workspace_matches
        && source_matches
        && vault_present
        && credential_present
        && source_active
        && initial_revision
        && ordinary_lineage
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
#[cfg(kani)]
const fn managed_creation_begin_allowed(source_published: bool, child_published: bool) -> bool {
    !source_published && !child_published
}

impl PendingManagedCredentialMutation {
    /// Validate the physical repository key before a decoded recovery envelope
    /// can reach the external SecretStore participant.
    pub fn validate_durable_key(&self, source_id: &str) -> Result<(), CredentialError> {
        validate_mutation_durable_key(source_id, &self.after_source.id)
    }

    /// Compare the complete secret-free logical command while ignoring only
    /// the source's random physical material-attempt namespace. The durable
    /// pending row remains the sole writer attempt when this returns true.
    #[must_use]
    pub fn matches_logical_command(&self, candidate: &Self) -> bool {
        self.operation_id == candidate.operation_id
            && self.operation == candidate.operation
            && self.before_source == candidate.before_source
            && logical_credential_source_matches(&self.after_source, &candidate.after_source)
            && self.before_credential == candidate.before_credential
            && self.after_credential == candidate.after_credential
    }

    pub fn claim_after_expiry(
        &self,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<Self>, CredentialError> {
        let Some(material_fence) = self
            .material_fence
            .claim_after_expiry(now_unix_ms, lease_expires_at_unix_ms)?
        else {
            return Ok(None);
        };
        let mut claimed = self.clone();
        claimed.material_fence = material_fence;
        Ok(Some(claimed))
    }

    pub fn with_material_ready(&self) -> Result<Self, CredentialError> {
        let mut ready = self.clone();
        ready.material_fence = self.material_fence.ready()?;
        Ok(ready)
    }

    pub fn with_material_reclaiming(&self) -> Result<Self, CredentialError> {
        let mut reclaiming = self.clone();
        reclaiming.material_fence = self.material_fence.reclaiming()?;
        Ok(reclaiming)
    }

    pub fn with_material_reclaiming_abort(&self) -> Result<Self, CredentialError> {
        let mut reclaiming = self.clone();
        reclaiming.material_fence = self.material_fence.reclaiming_abort()?;
        Ok(reclaiming)
    }

    /// New commands must use the current closed construction format. Legacy
    /// rows are accepted only through recovery after deserialization.
    pub fn validate_for_begin(&self) -> Result<(), ManagedCredentialMutationError> {
        self.validate()?;
        let before_refs = self
            .before_source
            .iter()
            .flat_map(CredentialSource::material_refs)
            .collect::<HashSet<_>>();
        let writes_new_material = self
            .after_source
            .material_refs()
            .any(|reference| !before_refs.contains(reference));
        let suffix = format!(":attempt:{}", self.material_fence.attempt_id());
        let new_refs_bound_to_attempt = !self.material_fence.attempt_id().is_empty()
            && self
                .after_source
                .material_refs()
                .filter(|reference| !before_refs.contains(reference))
                .all(|reference| reference.0.ends_with(&suffix));
        self.material_fence
            .validate_fresh_for_begin(writes_new_material, new_refs_bound_to_attempt)
            .map_err(|_| ManagedCredentialMutationError::RevisionConflict)
    }

    /// Revalidate the complete durable command shape at every persistence
    /// boundary. Public serde fields are transport data, never authority.
    pub fn validate(&self) -> Result<(), ManagedCredentialMutationError> {
        self.after_source.validate_authority()?;
        if let Some(before_source) = self.before_source.as_ref() {
            before_source.validate_authority()?;
        }
        if self.operation_id.trim().is_empty()
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
        let suffix = format!(":attempt:{}", self.material_fence.attempt_id());
        let new_refs_bound_to_attempt = !self.material_fence.attempt_id().is_empty()
            && self
                .after_source
                .material_refs()
                .filter(|reference| !before_refs.contains(reference))
                .all(|reference| reference.0.ends_with(&suffix));
        self.material_fence
            .validate(writes_new_material, new_refs_bound_to_attempt)
            .map_err(|_| ManagedCredentialMutationError::RevisionConflict)?;

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
                || self.after_source.replacement_of.is_some()
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
            && before_source.replacement_of == self.after_source.replacement_of
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
            source.replacement_of.is_none(),
        ) {
            return Err(CredentialError::InvalidSource(
                "Managed credential creation must freeze one exact active source/child pair".into(),
            ));
        }
        let writes_new_material = source.material_refs().next().is_some();
        let material_fence = CredentialMaterialMutationFence::fresh(writes_new_material)?;
        namespace_new_material_refs(None, &mut source, material_fence.attempt_id());
        Ok(Self {
            material_fence,
            operation_id: format!("managed-create:{}", source.id.0),
            operation: ManagedCredentialOperation::Create,
            before_source: None,
            after_source: source,
            before_credential: None,
            after_credential: credential,
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
        let material_fence = CredentialMaterialMutationFence::fresh(writes_new_material)
            .map_err(ManagedCredentialMutationError::Store)?;
        if writes_new_material {
            namespace_new_material_refs(
                Some(&before_source),
                &mut after_source,
                material_fence.attempt_id(),
            );
        }
        Ok(Self {
            material_fence,
            operation_id,
            operation,
            before_source: Some(before_source),
            after_source,
            before_credential: Some(before_credential),
            after_credential,
        })
    }

    #[must_use]
    pub fn source_id(&self) -> &CredentialSourceId {
        &self.after_source.id
    }
}

fn validate_mutation_durable_key(
    durable_source_id: &str,
    envelope_source_id: &CredentialSourceId,
) -> Result<(), CredentialError> {
    if durable_source_id == envelope_source_id.0 {
        Ok(())
    } else {
        Err(CredentialError::Storage(format!(
            "credential mutation row key `{durable_source_id}` does not match envelope source `{}`",
            envelope_source_id.0
        )))
    }
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
        kani::any::<bool>(),
    ];
    let admitted = managed_creation_pair_admitted(
        axes[0], axes[1], axes[2], axes[3], axes[4], axes[5], axes[6],
    );
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

/// One physical persistence boundary for every Managed Credential command.
/// Implementations publish Source + child/tombstone and advance the durable
/// fact to `Reclaiming` in one local transaction.
#[async_trait::async_trait]
pub trait ManagedCredentialRepository: CredentialRepo + ManagedVaultRepo {
    /// Return `true` only to the command that inserted and owns the durable
    /// physical attempt. A logically identical follower returns `false` and
    /// must perform zero SecretStore writes; a different command conflicts.
    async fn begin_managed_mutation(
        &self,
        pending: PendingManagedCredentialMutation,
    ) -> Result<bool, CredentialError>;
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
    /// Optional canonical descriptor compiled by the target-owning application
    /// before the aggregate enters its WAL/CAS publication path.
    pub descriptor: Option<awaken_credential_contract::CredentialDescriptor>,
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

    /// Durably claim this exact source-only publication envelope. New commands
    /// must carry the current material-fence format. `true` owns the inserted
    /// physical attempt; a logically identical live retry returns `false`, does
    /// not acquire another writer token, and must perform zero SecretStore writes.
    async fn begin_mutation(
        &self,
        intent: CredentialMutationIntent,
    ) -> Result<bool, CredentialError>;
    /// Fence a completed external write before publication.
    async fn mark_mutation_ready(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError>;
    /// Atomically compare/publish the source and advance the exact durable fact
    /// to `Reclaiming`; external cleanup is forbidden before this returns.
    async fn apply_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError>;
    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError>;
    async fn claim_expired_mutation(
        &self,
        intent: &CredentialMutationIntent,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<CredentialMutationIntent>, CredentialError>;
    /// Atomically retain exact unpublished truth as `ReclaimingAbort` before
    /// any candidate material is deleted.
    async fn abort_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError>;
    /// Remove only an exact terminal cleanup fact whose published/unpublished
    /// source truth still matches its phase.
    async fn complete_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError>;
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
        intent.validate_for_begin()?;
        let mut state = self.state.lock().expect("credential repo");
        if let Some(durable) = state.intents.get(&intent.after.id.0) {
            return if durable.matches_logical_command(&intent) {
                Ok(false)
            } else {
                Err(CredentialError::MutationConflict(
                    "another credential mutation is pending".into(),
                ))
            };
        }
        let current_after = state.rows.get(&intent.after.id.0);
        let before_matches = if intent.is_distinct_replacement() {
            current_after.is_none()
                && intent
                    .before
                    .as_ref()
                    .is_some_and(|before| state.rows.get(&before.id.0) == Some(before))
        } else {
            current_after == intent.before.as_ref()
        };
        if !before_matches {
            return Err(CredentialError::MutationConflict(
                "credential changed before its material mutation was prepared".into(),
            ));
        }
        state.intents.insert(intent.after.id.0.clone(), intent);
        Ok(true)
    }

    async fn mark_mutation_ready(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError> {
        intent.validate()?;
        let ready = intent.with_material_ready()?;
        let mut state = self.state.lock().expect("credential repo");
        let durable = state.intents.get_mut(&intent.after.id.0).ok_or_else(|| {
            CredentialError::MutationConflict(
                "credential material mutation has no durable pending fact".into(),
            )
        })?;
        if durable != intent {
            return Err(CredentialError::MutationConflict(
                "credential ready transition does not match its durable Writing owner".into(),
            ));
        }
        *durable = ready.clone();
        Ok(ready)
    }

    async fn apply_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError> {
        intent.validate()?;
        if intent.material_fence.phase != CredentialMaterialMutationPhase::Ready {
            return Err(CredentialError::MutationConflict(
                "credential material mutation is not ready to publish".into(),
            ));
        }
        let mut state = self.state.lock().expect("credential repo");
        let expected_reclaiming = intent.with_material_reclaiming()?;
        if state.intents.get(&intent.after.id.0) == Some(&expected_reclaiming)
            && state.rows.get(&intent.after.id.0) == Some(&intent.after)
        {
            return Ok(expected_reclaiming);
        }
        if state.intents.get(&intent.after.id.0) != Some(intent) {
            return Err(CredentialError::MutationConflict(
                "credential mutation has no matching durable intent".into(),
            ));
        }
        let current_after = state.rows.get(&intent.after.id.0);
        if intent.is_distinct_replacement() {
            let before = intent.before.as_ref().ok_or_else(|| {
                CredentialError::InvalidSource(
                    "distinct replacement requires an exact predecessor".into(),
                )
            })?;
            if current_after.is_some() || state.rows.get(&before.id.0) != Some(before) {
                return Err(CredentialError::MutationConflict(
                    "credential replacement precondition changed during mutation".into(),
                ));
            }
            state
                .rows
                .insert(intent.after.id.0.clone(), intent.after.clone());
        } else if current_after != intent.before.as_ref() {
            return Err(CredentialError::MutationConflict(
                "credential revision changed during mutation".into(),
            ));
        } else {
            state
                .rows
                .insert(intent.after.id.0.clone(), intent.after.clone());
        }
        state
            .intents
            .insert(intent.after.id.0.clone(), expected_reclaiming.clone());
        Ok(expected_reclaiming)
    }

    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .intents
            .iter()
            .map(|(source_id, intent)| {
                intent.validate_durable_key(source_id)?;
                Ok(intent.clone())
            })
            .collect()
    }

    async fn claim_expired_mutation(
        &self,
        intent: &CredentialMutationIntent,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<CredentialMutationIntent>, CredentialError> {
        let Some(claimed) = intent.claim_after_expiry(now_unix_ms, lease_expires_at_unix_ms)?
        else {
            return Ok(None);
        };
        let mut state = self.state.lock().expect("credential repo");
        let Some(durable) = state.intents.get_mut(&intent.after.id.0) else {
            return Ok(None);
        };
        if durable != intent
            || durable.material_fence.phase != CredentialMaterialMutationPhase::Writing
        {
            return Ok(None);
        }
        *durable = claimed.clone();
        Ok(Some(claimed))
    }

    async fn abort_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError> {
        intent.validate()?;
        let abort = intent.with_material_reclaiming_abort()?;
        let mut state = self.state.lock().expect("credential repo");
        if state.intents.get(&intent.after.id.0) == Some(&abort) {
            return Ok(abort);
        }
        let unpublished = if intent.is_distinct_replacement() {
            !state.rows.contains_key(&intent.after.id.0)
        } else {
            state.rows.get(&intent.after.id.0) == intent.before.as_ref()
        };
        if !unpublished || state.intents.get(&intent.after.id.0) != Some(intent) {
            return Err(CredentialError::MutationConflict(
                "credential abort does not match exact unpublished truth".into(),
            ));
        }
        state
            .intents
            .insert(intent.after.id.0.clone(), abort.clone());
        Ok(abort)
    }

    async fn complete_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        intent.validate()?;
        let mut state = self.state.lock().expect("credential repo");
        let Some(durable) = state.intents.get(&intent.after.id.0) else {
            return Ok(());
        };
        let truth_matches_phase = match intent.material_fence.phase {
            CredentialMaterialMutationPhase::Reclaiming => {
                state.rows.get(&intent.after.id.0) == Some(&intent.after)
            }
            CredentialMaterialMutationPhase::ReclaimingAbort => {
                if intent.is_distinct_replacement() {
                    !state.rows.contains_key(&intent.after.id.0)
                } else {
                    state.rows.get(&intent.after.id.0) == intent.before.as_ref()
                }
            }
            CredentialMaterialMutationPhase::Writing | CredentialMaterialMutationPhase::Ready => {
                false
            }
        };
        if durable != intent || !truth_matches_phase {
            return Err(CredentialError::MutationConflict(
                "credential completion does not match durable cleanup truth".into(),
            ));
        }
        state.intents.remove(&intent.after.id.0);
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
    ) -> Result<bool, CredentialError> {
        pending
            .validate_for_begin()
            .map_err(invalid_pending_mutation)?;
        let mut state = self.state.lock().expect("credential repo");
        if let Some(durable) = state.managed_mutations.get(&pending.after_source.id.0) {
            return if durable.matches_logical_command(&pending) {
                Ok(false)
            } else {
                Err(CredentialError::MutationConflict(
                    "another Managed credential mutation is pending".into(),
                ))
            };
        }
        let current_source = state.rows.get(&pending.after_source.id.0);
        let current_child = state.vault_credentials.get(&pending.after_credential.id);
        if current_source != pending.before_source.as_ref()
            || current_child != pending.before_credential.as_ref()
        {
            return Err(CredentialError::MutationConflict(
                "Managed credential changed before its mutation was prepared".into(),
            ));
        }
        state
            .managed_mutations
            .insert(pending.after_source.id.0.clone(), pending);
        Ok(true)
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
        durable.validate()?;
        if durable.material_fence.phase == CredentialMaterialMutationPhase::Reclaiming {
            let expected_reclaiming = pending.with_material_reclaiming()?;
            if durable == expected_reclaiming
                && state.rows.get(&pending.after_source.id.0) == Some(&pending.after_source)
                && state.vault_credentials.get(&pending.after_credential.id)
                    == Some(&pending.after_credential)
            {
                return Ok(durable);
            }
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        if &durable != pending
            || pending.material_fence.phase != CredentialMaterialMutationPhase::Ready
        {
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
        let reclaiming = pending
            .with_material_reclaiming()
            .map_err(ManagedCredentialMutationError::Store)?;
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
        if durable != pending
            || pending.material_fence.phase != CredentialMaterialMutationPhase::Writing
        {
            return Err(CredentialError::MutationConflict(
                "Managed credential ready transition does not match Writing".into(),
            ));
        }
        let ready = pending.with_material_ready()?;
        *durable = ready.clone();
        Ok(ready)
    }

    async fn pending_managed_mutations(
        &self,
    ) -> Result<Vec<PendingManagedCredentialMutation>, CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .managed_mutations
            .iter()
            .map(|(source_id, pending)| {
                pending.validate_durable_key(source_id)?;
                Ok(pending.clone())
            })
            .collect()
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
        if durable != pending
            || durable.material_fence.phase != CredentialMaterialMutationPhase::Writing
        {
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
        if durable == pending
            && pending.material_fence.phase == CredentialMaterialMutationPhase::ReclaimingAbort
        {
            return Ok(durable.clone());
        }
        if durable != pending
            || !matches!(
                pending.material_fence.phase,
                CredentialMaterialMutationPhase::Writing | CredentialMaterialMutationPhase::Ready
            )
        {
            return Err(CredentialError::MutationConflict(
                "Managed credential abort does not match its durable pending fact".into(),
            ));
        }
        let reclaiming = pending.with_material_reclaiming_abort()?;
        *durable = reclaiming.clone();
        Ok(reclaiming)
    }

    async fn complete_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<(), CredentialError> {
        pending.validate().map_err(invalid_pending_mutation)?;
        let mut state = self.state.lock().expect("credential repo");
        let durable = state.managed_mutations.get(&pending.after_source.id.0);
        if durable.is_none() {
            return Ok(());
        }
        let truth_matches_phase = match pending.material_fence.phase {
            CredentialMaterialMutationPhase::Reclaiming => {
                state.rows.get(&pending.after_source.id.0) == Some(&pending.after_source)
                    && state.vault_credentials.get(&pending.after_credential.id)
                        == Some(&pending.after_credential)
            }
            CredentialMaterialMutationPhase::ReclaimingAbort => {
                state.rows.get(&pending.after_source.id.0) == pending.before_source.as_ref()
                    && state.vault_credentials.get(&pending.after_credential.id)
                        == pending.before_credential.as_ref()
            }
            CredentialMaterialMutationPhase::Writing | CredentialMaterialMutationPhase::Ready => {
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
