// @formal entrypoint credential.managed_creation_begin_rejects_every_published_source_or_child_identity
// @formal entrypoint credential.managed_creation_pair_requires_exact_workspace_source_and_initial_lifecycle
// @formal entrypoint credential.managed_vault_child_admission_requires_active_parent_capacity_and_unique_key
// @formal entrypoint credential.managed_vault_delete_fences_children_waits_for_rollout_and_is_absorbing
// @formal entrypoint credential.managed_vault_replacement_rejects_stale_and_non_successor_revisions
// @formal entrypoint credential.managed_rollout_abstract_ack_requires_every_target_adoption
//! The Vault use cases of the credential context: Vault and credential
//! lifecycle commands, credential material ingress, application MCP bearer
//! entry, rollout delivery and exact credential access, over one secret store,
//! one credential repository and one wall clock.
//!
//! A protocol adapter maps its wire shapes onto these commands and projects
//! the returned records; it owns no store, no clock and no orchestration. MCP
//! target normalization stays with the caller (it is Session vocabulary): the
//! commands take the canonical audience and target fingerprint it computed.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{
    CredentialAccess, CredentialDescriptor, CredentialMaterialDelivery,
    CredentialMaterialDescriptor, CredentialPurpose, CredentialRef, CredentialRefreshAccess,
    CredentialSourceId, CredentialTarget, CredentialTargetContract, CredentialUsage,
    OPAQUE_SECRET_MATERIAL_TYPE, TokenEndpointAuth,
};
use awaken_time::WallClock;
use sha2::{Digest, Sha256};

use crate::catalog::{
    ManagedCredentialAdmissionError, ManagedCredentialAuth, ManagedMcpOauthRefresh, ManagedVault,
    ManagedVaultCredential, ManagedVaultMutationError, request_managed_vault_deletion,
};
use crate::repo::{
    ApplicationMcpBearerCommand, CredentialMaterialPatch, CredentialRetirement,
    ManagedCredentialCreateCommand, ManagedCredentialCreationError, ManagedCredentialOperation,
    ManagedCredentialRepository, application_mcp_material_ref, application_mcp_operation_id,
    create_managed_credential, prepare_application_mcp_bearer_rotation,
    reconcile_managed_credential_rollout, reconcile_managed_vault_deletion,
    retire_managed_credential, revoke_credential_exact, rotate_credential_materials_exact,
    update_managed_credential, update_managed_credential_prepared,
};
use crate::{
    CredentialCreateParams, CredentialError, CredentialKind, CredentialSource, CredentialStatus,
    ExactCredentialAccessRequest, OAUTH_CLIENT_SECRET_SLOT, OAUTH_REFRESH_TOKEN_SLOT, SecretStore,
};
use awaken_credential_contract::{
    ManagedCredentialAdoptionProgress, ManagedCredentialRolloutTarget,
};

/// Which Vault object a command could not find in the caller's Workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultObject {
    Vault,
    Credential,
}

/// Why a Vault command did not complete. Each variant keeps the domain error
/// that decided it, so a protocol maps it to its own wire envelope.
#[derive(Debug, thiserror::Error)]
pub enum VaultCommandError {
    #[error("{0:?} was not found")]
    NotFound(VaultObject),
    #[error(transparent)]
    Store(#[from] CredentialError),
    #[error(transparent)]
    Vault(#[from] ManagedVaultMutationError),
    #[error(transparent)]
    Creation(#[from] ManagedCredentialCreationError),
    #[error(transparent)]
    Credential(#[from] crate::catalog::ManagedCredentialMutationError),
}

/// The normalized MCP endpoint an application bearer authorizes, computed by
/// the caller from the server URL: the canonical audience and the stable
/// normalized-target fingerprint that keys the credential identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCredentialTarget {
    pub audience: String,
    pub fingerprint: String,
}

/// One hosted application's request to create or rotate its stable MCP
/// bearer.
#[derive(Debug, Clone)]
pub struct ApplicationMcpBearerEntry {
    pub workspace_id: String,
    pub application_authority_id: String,
    pub mcp_server_url: String,
    pub target: McpCredentialTarget,
    pub idempotency_key: String,
    pub credential_generation: u64,
    pub bearer: RedactedString,
}

/// The secret-free facts an application bearer entry committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationMcpBearerReceipt {
    pub vault_id: String,
    pub source_id: CredentialSourceId,
    pub revision: u64,
    pub adoption: ManagedCredentialAdoptionProgress,
}

/// Everything a new Vault credential carries except its identity, parent and
/// creation instant, which the application derives. It carries write-only
/// material, so it is neither printed nor copied.
pub struct VaultCredentialDraft {
    pub source: CredentialCreateParams,
    pub descriptor: Option<CredentialDescriptor>,
    pub auxiliary_materials: BTreeMap<String, RedactedString>,
    pub auth: ManagedCredentialAuth,
    pub metadata: BTreeMap<String, String>,
    pub display_name: Option<String>,
}

/// One authored change of a live credential: its secret-free record after the
/// change, the write-only material patch, and whether the source revision
/// advances although no material changed (a refresh configuration change).
pub struct VaultCredentialChange {
    pub before: ManagedVaultCredential,
    pub after: ManagedVaultCredential,
    pub material: CredentialMaterialPatch,
    pub advance_source_without_material: bool,
}

/// The receipt of one consumer-neutral credential material entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialEntry {
    pub credential: CredentialRef,
    pub created: bool,
}

pub struct VaultApplication {
    secrets: Arc<dyn SecretStore>,
    repository: Arc<dyn ManagedCredentialRepository>,
    material_delivery: Option<CredentialMaterialDelivery>,
    rollout_target: RwLock<Option<Arc<dyn ManagedCredentialRolloutTarget>>>,
    /// The one wall clock this owner samples: object creation/update/archive
    /// and deletion-request instants, validation instants, and the `now` of
    /// exact credential access. Tests install a fixed clock.
    clock: Arc<dyn WallClock>,
}

impl VaultApplication {
    /// The Vault use cases over the credential domain's secret store and
    /// repository. `clock` is the composing process's one wall clock.
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        repository: Arc<dyn ManagedCredentialRepository>,
        clock: Arc<dyn WallClock>,
    ) -> Self {
        Self {
            secrets,
            repository,
            material_delivery: None,
            rollout_target: RwLock::new(None),
            clock,
        }
    }

    /// Install the one deployment-owned plaintext delivery mechanism.
    /// Selection, revision, holder, usage and target binding remain
    /// Vault/Session facts.
    #[must_use]
    pub fn with_material_delivery(mut self, delivery: CredentialMaterialDelivery) -> Self {
        self.material_delivery = Some(delivery);
        self
    }

    /// Sample the owner clock once. The epoch itself is the system clock's
    /// failure value, so it fails closed instead of backdating expiry checks
    /// or stamping a fabricated instant on a durable object.
    pub fn clock_reading_ms(&self) -> Result<u64, CredentialError> {
        crate::repo::credential_material_now_unix_ms(self.clock.as_ref())
    }

    /// The owner clock as the RFC 3339 wire/persistence instant.
    pub fn clock_reading_rfc3339(&self) -> Result<String, CredentialError> {
        self.clock_reading_ms()
            .map(awaken_time::utc::epoch_millis_to_rfc3339)
    }

    // ---- Rollout delivery ----------------------------------------------------

    /// Install the service-owned rolling replacement edge after process
    /// composition has constructed both Control and Session applications.
    pub fn set_rollout_target(&self, target: Arc<dyn ManagedCredentialRolloutTarget>) {
        *self.rollout_target.write().expect("Vault rollout target") = Some(target);
    }

    fn rollout_target(&self) -> Option<Arc<dyn ManagedCredentialRolloutTarget>> {
        self.rollout_target
            .read()
            .expect("Vault rollout target")
            .clone()
    }

    /// Best-effort one delivery pass. Durable events remain pending when no
    /// target is available (split-service startup) or a Session is still busy.
    pub async fn reconcile_rollouts(&self) -> Result<usize, CredentialError> {
        let Some(target) = self.rollout_target() else {
            return Ok(0);
        };
        crate::repo::reconcile_managed_credential_rollouts(
            self.repository.as_ref(),
            target.as_ref(),
        )
        .await
    }

    async fn reconcile_application_mcp_rollout(
        &self,
        event_id: Option<&str>,
        workspace_id: &str,
        vault_id: &str,
        credential_id: &str,
        source_id: &CredentialSourceId,
        source_revision: u64,
    ) -> Result<ManagedCredentialAdoptionProgress, CredentialError> {
        let Some(event_id) = event_id else {
            // Managed creation has no predecessor to replace and therefore no
            // rollout.
            return Ok(ManagedCredentialAdoptionProgress::Converged);
        };
        let Some(event) = self.repository.managed_rollout(event_id).await? else {
            // An absent exact update event has already been exactly
            // acknowledged by this same repository.
            return Ok(ManagedCredentialAdoptionProgress::Converged);
        };
        if event.workspace_id != workspace_id
            || event.vault_id != vault_id
            || event.credential_id != credential_id
            || event.source_id != *source_id
            || event.source_version != source_revision
            || event.operation != ManagedCredentialOperation::Update
        {
            return Err(CredentialError::MutationConflict(
                "application MCP rollout identity conflicts with committed credential".into(),
            ));
        }
        let Some(target) = self.rollout_target() else {
            return Ok(ManagedCredentialAdoptionProgress::Pending);
        };
        reconcile_managed_credential_rollout(&event, self.repository.as_ref(), target.as_ref())
            .await
    }

    // ---- Application MCP bearer ----------------------------------------------

    /// Create or rotate one hosted application's stable MCP bearer in the
    /// authoritative credential aggregate. HTTP ownership stays with Awaken
    /// Control; this command returns only secret-free receipt facts.
    pub async fn enter_application_mcp_bearer(
        &self,
        entry: ApplicationMcpBearerEntry,
    ) -> Result<ApplicationMcpBearerReceipt, CredentialError> {
        let ApplicationMcpBearerEntry {
            workspace_id,
            application_authority_id,
            mcp_server_url,
            target,
            idempotency_key,
            credential_generation,
            bearer,
        } = entry;
        let workspace_id = workspace_id.as_str();
        let target_fingerprint = target.fingerprint;
        let vault_id = application_mcp_vault_id(workspace_id, &application_authority_id);
        let source_id = application_mcp_source_id(&vault_id, &target_fingerprint);
        let command_key_fingerprint = identity_digest(
            "application-mcp-command-key-v1",
            &[
                workspace_id,
                &application_authority_id,
                &target_fingerprint,
                &idempotency_key,
            ],
        );
        let material_ref = application_mcp_material_ref(
            &source_id,
            &command_key_fingerprint,
            credential_generation,
        )?;
        let descriptor = mcp_bearer_descriptor(&target.audience);
        let target_audience = target.audience;
        let now = self.clock_reading_rfc3339()?;
        self.repository
            .ensure_vault(
                workspace_id,
                ManagedVault {
                    id: vault_id.clone(),
                    workspace_id: workspace_id.to_owned(),
                    display_name: application_authority_id.clone(),
                    metadata: BTreeMap::new(),
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    archived_at: None,
                    deletion: None,
                    revision: 1,
                },
            )
            .await?;
        let credential_id = format!(
            "crd_app_{}",
            identity_digest("application-mcp-credential-v1", &[&source_id.0])
        );
        let atomically_created = match self.repository.get(&source_id).await {
            Err(CredentialError::SourceNotFound(_)) => {
                match create_managed_credential(
                    ManagedCredentialCreateCommand {
                        source: CredentialCreateParams {
                            workspace_id: workspace_id.to_owned(),
                            kind: CredentialKind::Vault,
                            provider_id: None,
                            env_key: None,
                            secret: Some(bearer.clone()),
                            oauth_command: None,
                        },
                        descriptor: Some(descriptor),
                        source_id: Some(source_id.clone()),
                        protocol_endpoint_id: Some(target_fingerprint.clone()),
                        primary_material_ref: Some(material_ref),
                        auxiliary_materials: BTreeMap::new(),
                        credential_id: credential_id.clone(),
                        vault_id: vault_id.clone(),
                        auth: ManagedCredentialAuth::StaticBearer {
                            mcp_server_url: mcp_server_url.clone(),
                        },
                        metadata: BTreeMap::new(),
                        display_name: None,
                        created_at: now.clone(),
                    },
                    self.secrets.as_ref(),
                    self.repository.as_ref(),
                    self.clock.as_ref(),
                )
                .await
                {
                    Ok((source, _)) => Some(source),
                    Err(ManagedCredentialCreationError::Credential(
                        CredentialError::MutationConflict(_),
                    ))
                    | Err(ManagedCredentialCreationError::Admission(
                        ManagedCredentialAdmissionError::Store(CredentialError::MutationConflict(
                            _,
                        )),
                    )) => None,
                    Err(ManagedCredentialCreationError::Credential(error))
                    | Err(ManagedCredentialCreationError::Admission(
                        ManagedCredentialAdmissionError::Store(error),
                    )) => return Err(error),
                    Err(ManagedCredentialCreationError::Admission(error)) => {
                        return Err(CredentialError::InvalidSource(error.to_string()));
                    }
                }
            }
            Ok(_) => None,
            Err(error) => return Err(error),
        };
        let (source, rollout_id) = if let Some(source) = atomically_created {
            (source, None)
        } else {
            self.rotate_application_mcp_bearer(
                ApplicationMcpBearerCommand {
                    source_id: source_id.clone(),
                    workspace_id: workspace_id.to_owned(),
                    target_fingerprint,
                    target_audience,
                    command_key_fingerprint,
                    credential_generation,
                    bearer,
                },
                now,
            )
            .await?
        };
        let revision = u64::try_from(source.version).map_err(|_| {
            CredentialError::InvalidSource("application MCP credential revision is invalid".into())
        })?;
        let adoption = self
            .reconcile_application_mcp_rollout(
                rollout_id.as_deref(),
                workspace_id,
                &vault_id,
                &credential_id,
                &source.id,
                revision,
            )
            .await?;
        Ok(ApplicationMcpBearerReceipt {
            vault_id,
            source_id: source.id,
            revision,
            adoption,
        })
    }

    /// Rotate (or recognize the replay of) an existing application bearer:
    /// the committed source and the rollout its revision opened, if any.
    async fn rotate_application_mcp_bearer(
        &self,
        command: ApplicationMcpBearerCommand,
        now: String,
    ) -> Result<(CredentialSource, Option<String>), CredentialError> {
        let source_id = command.source_id.clone();
        let credential_generation = command.credential_generation;
        let replay_command_key_fingerprint = command.command_key_fingerprint.clone();
        let before_source = self.repository.get(&source_id).await?;
        let before_credential = self
            .repository
            .get_vault_credential_by_source(&command.workspace_id, &source_id)
            .await?
            .ok_or_else(|| {
                CredentialError::MutationConflict(
                    "application MCP Source has no matching Managed child".into(),
                )
            })?;
        let rotation =
            prepare_application_mcp_bearer_rotation(command, &before_source, self.secrets.as_ref())
                .await?;
        match rotation {
            None => {
                let rollout_id = before_source
                    .version
                    .checked_sub(1)
                    .filter(|prior_version| *prior_version > 0)
                    .map(|prior_version| {
                        application_mcp_operation_id(
                            &source_id,
                            prior_version,
                            &replay_command_key_fingerprint,
                            credential_generation,
                        )
                    })
                    .transpose()?;
                Ok((before_source, rollout_id))
            }
            Some(rotation) => {
                let rollout_id = rotation.operation_id.clone();
                let source = update_managed_credential_prepared(
                    before_credential.clone(),
                    before_credential,
                    before_source,
                    rotation,
                    now,
                    self.secrets.as_ref(),
                    self.repository.as_ref(),
                    self.clock.as_ref(),
                )
                .await
                .map_err(|error| CredentialError::MutationConflict(error.to_string()))?
                .0;
                Ok((source, Some(rollout_id)))
            }
        }
    }

    // ---- Vault lifecycle -----------------------------------------------------

    /// Create one Vault in the caller's Workspace.
    pub async fn create_vault(
        &self,
        workspace_id: &str,
        display_name: String,
        metadata: BTreeMap<String, String>,
    ) -> Result<ManagedVault, VaultCommandError> {
        let now = self.clock_reading_rfc3339()?;
        let record = ManagedVault {
            id: format!("vlt_{}", uuid::Uuid::now_v7().simple()),
            workspace_id: workspace_id.to_owned(),
            display_name,
            metadata,
            created_at: now.clone(),
            updated_at: now,
            archived_at: None,
            deletion: None,
            revision: 1,
        };
        self.repository
            .insert_vault(workspace_id, record.clone())
            .await?;
        Ok(record)
    }

    /// One Vault that is not being deleted.
    pub async fn visible_vault(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<ManagedVault, VaultCommandError> {
        self.repository
            .get_vault(workspace_id, id)
            .await?
            .filter(|record| record.deletion.is_none())
            .ok_or(VaultCommandError::NotFound(VaultObject::Vault))
    }

    /// The Workspace's Vaults that are not being deleted, archived ones only
    /// when asked, in ascending id order (`vlt_…` ids sort in creation order).
    pub async fn visible_vaults(
        &self,
        workspace_id: &str,
        include_archived: bool,
    ) -> Result<Vec<ManagedVault>, CredentialError> {
        let mut records = self.repository.list_vaults(workspace_id).await?;
        records.retain(|record| {
            record.deletion.is_none() && (include_archived || record.archived_at.is_none())
        });
        records.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(records)
    }

    /// Archive a Vault; an archived one is returned unchanged. Archiving does
    /// not cascade to its credentials.
    pub async fn archive_vault(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<ManagedVault, VaultCommandError> {
        let mut record = self.visible_vault(workspace_id, id).await?;
        if record.archived_at.is_some() {
            return Ok(record);
        }
        let expected_revision = record.revision;
        let archived_at = self.clock_reading_rfc3339()?;
        record.updated_at.clone_from(&archived_at);
        record.archived_at = Some(archived_at);
        record.revision = expected_revision
            .checked_add(1)
            .ok_or(ManagedVaultMutationError::RevisionExhausted)?;
        self.repository
            .replace_vault(workspace_id, expected_revision, record.clone())
            .await?;
        Ok(record)
    }

    /// Rename a Vault and patch its metadata; an unchanged Vault is returned
    /// without a new revision.
    pub async fn update_vault(
        &self,
        workspace_id: &str,
        id: &str,
        display_name: Option<String>,
        metadata: Option<BTreeMap<String, Option<String>>>,
    ) -> Result<ManagedVault, VaultCommandError> {
        let mut record = self.visible_vault(workspace_id, id).await?;
        let before = record.clone();
        let expected_revision = record.revision;
        if let Some(name) = display_name {
            record.display_name = name;
        }
        if let Some(patch) = metadata {
            apply_metadata_patch(&mut record.metadata, patch);
        }
        if record == before {
            return Ok(record);
        }
        record.updated_at = self.clock_reading_rfc3339()?;
        record.revision = expected_revision
            .checked_add(1)
            .ok_or(ManagedVaultMutationError::RevisionExhausted)?;
        self.repository
            .replace_vault(workspace_id, expected_revision, record.clone())
            .await?;
        Ok(record)
    }

    /// Delete a Vault: publish the durable delete fence first, then retire its
    /// credentials. Cancellation or a transient failure after the fence leaves
    /// a supervised, retryable root fact.
    pub async fn delete_vault(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<(), VaultCommandError> {
        let current = self
            .repository
            .get_vault(workspace_id, id)
            .await?
            .ok_or(VaultCommandError::NotFound(VaultObject::Vault))?;
        let requested_at = self.clock_reading_rfc3339()?;
        let (requested, changed) = request_managed_vault_deletion(&current, requested_at)?;
        let requested = self
            .persist_vault_deletion_request(workspace_id, &current, requested, changed)
            .await?;
        let _completed = reconcile_managed_vault_deletion(
            &requested,
            self.secrets.as_ref(),
            self.repository.as_ref(),
            self.clock.as_ref(),
        )
        .await?;
        Ok(())
    }

    /// Publish the root delete fence, or resume the durable winner when another
    /// request committed the fence after this command read its snapshot.
    async fn persist_vault_deletion_request(
        &self,
        workspace_id: &str,
        current: &ManagedVault,
        requested: ManagedVault,
        changed: bool,
    ) -> Result<ManagedVault, VaultCommandError> {
        if !changed {
            return Ok(requested);
        }
        match self
            .repository
            .replace_vault(workspace_id, current.revision, requested.clone())
            .await
        {
            Ok(()) => Ok(requested),
            Err(error)
                if matches!(
                    error,
                    ManagedVaultMutationError::RevisionConflict
                        | ManagedVaultMutationError::InvalidLifecycle
                ) =>
            {
                match self.repository.get_vault(workspace_id, &current.id).await? {
                    Some(durable) if durable.deletion.is_some() => Ok(durable),
                    _ => Err(error.into()),
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    // ---- Credential lifecycle ------------------------------------------------

    /// Create one credential in `vault_id`. With an idempotency key the
    /// credential and source identities derive from (Workspace, Vault, key),
    /// so a retried request names the same objects; without one they are new.
    pub async fn create_credential(
        &self,
        workspace_id: &str,
        vault_id: String,
        idempotency_key: Option<&str>,
        draft: VaultCredentialDraft,
    ) -> Result<ManagedVaultCredential, VaultCommandError> {
        let command_identity = idempotency_key.map(|key| {
            identity_digest(
                "managed-vault-credential-create-v1",
                &[workspace_id, &vault_id, key],
            )
        });
        let credential_id = command_identity.as_ref().map_or_else(
            || format!("crd_{}", uuid::Uuid::now_v7().simple()),
            |identity| format!("crd_{}", &identity[..32]),
        );
        let VaultCredentialDraft {
            source,
            descriptor,
            auxiliary_materials,
            auth,
            metadata,
            display_name,
        } = draft;
        let command = ManagedCredentialCreateCommand {
            source,
            descriptor,
            source_id: command_identity
                .as_ref()
                .map(|identity| CredentialSourceId(format!("cred:managed:{identity}"))),
            protocol_endpoint_id: None,
            primary_material_ref: None,
            auxiliary_materials,
            credential_id,
            vault_id,
            auth,
            metadata,
            display_name,
            created_at: self.clock_reading_rfc3339()?,
        };
        let (_, record) = create_managed_credential(
            command,
            self.secrets.as_ref(),
            self.repository.as_ref(),
            self.clock.as_ref(),
        )
        .await?;
        Ok(record)
    }

    /// The credentials of one Vault that are not deleted, archived ones only
    /// when asked, in ascending id order. An unknown Vault is not found.
    pub async fn visible_credentials(
        &self,
        workspace_id: &str,
        vault_id: &str,
        include_archived: bool,
    ) -> Result<Vec<ManagedVaultCredential>, VaultCommandError> {
        if self
            .repository
            .get_vault(workspace_id, vault_id)
            .await?
            .is_none()
        {
            return Err(VaultCommandError::NotFound(VaultObject::Vault));
        }
        let mut records = self
            .repository
            .list_vault_credentials(workspace_id, vault_id)
            .await?;
        records.retain(|record| {
            !record.lifecycle.is_deleted() && (include_archived || record.lifecycle.is_active())
        });
        records.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(records)
    }

    /// One credential of `vault_id` that is not deleted.
    pub async fn visible_credential(
        &self,
        workspace_id: &str,
        vault_id: &str,
        id: &str,
    ) -> Result<ManagedVaultCredential, VaultCommandError> {
        self.repository
            .get_vault_credential(workspace_id, id)
            .await?
            .filter(|record| record.vault_id == vault_id && !record.lifecycle.is_deleted())
            .ok_or(VaultCommandError::NotFound(VaultObject::Credential))
    }

    /// One active credential of `vault_id`: the only kind a change or a
    /// validation applies to.
    pub async fn active_credential(
        &self,
        workspace_id: &str,
        vault_id: &str,
        id: &str,
    ) -> Result<ManagedVaultCredential, VaultCommandError> {
        self.repository
            .get_vault_credential(workspace_id, id)
            .await?
            .filter(|record| record.vault_id == vault_id && record.lifecycle.is_active())
            .ok_or(VaultCommandError::NotFound(VaultObject::Credential))
    }

    /// Logically delete one credential of `vault_id`. The absorbing tombstone
    /// remains as a stale-writer fence while sealed material is reclaimed.
    pub async fn delete_credential(
        &self,
        workspace_id: &str,
        vault_id: &str,
        id: &str,
    ) -> Result<(), VaultCommandError> {
        self.visible_credential(workspace_id, vault_id, id).await?;
        self.retire_credential(
            workspace_id,
            vault_id,
            id,
            ManagedCredentialOperation::Delete,
        )
        .await
        .map(|_| ())
    }

    /// Archive one credential of `vault_id` and return its record.
    pub async fn archive_credential(
        &self,
        workspace_id: &str,
        vault_id: &str,
        id: &str,
    ) -> Result<ManagedVaultCredential, VaultCommandError> {
        self.retire_credential(
            workspace_id,
            vault_id,
            id,
            ManagedCredentialOperation::Archive,
        )
        .await
    }

    async fn retire_credential(
        &self,
        workspace_id: &str,
        vault_id: &str,
        id: &str,
        operation: ManagedCredentialOperation,
    ) -> Result<ManagedVaultCredential, VaultCommandError> {
        Ok(retire_managed_credential(
            workspace_id,
            vault_id,
            id,
            operation,
            self.clock_reading_rfc3339()?,
            self.secrets.as_ref(),
            self.repository.as_ref(),
            self.clock.as_ref(),
        )
        .await?)
    }

    /// Commit one authored credential change as one exact revision; a change
    /// that alters nothing returns the record unchanged.
    pub async fn change_credential(
        &self,
        change: VaultCredentialChange,
    ) -> Result<ManagedVaultCredential, VaultCommandError> {
        let VaultCredentialChange {
            before,
            after,
            material,
            advance_source_without_material,
        } = change;
        if after == before
            && material.primary.is_none()
            && material.auxiliary.is_empty()
            && !advance_source_without_material
        {
            return Ok(after);
        }
        Ok(update_managed_credential(
            before,
            after,
            material,
            advance_source_without_material,
            self.clock_reading_rfc3339()?,
            self.secrets.as_ref(),
            self.repository.as_ref(),
            self.clock.as_ref(),
        )
        .await?)
    }

    // ---- Reads for Session binding and MCP inspection ------------------------

    /// Whether `id` names an active vault that may be attached to a new Session.
    /// Archived vaults remain retrievable for audit but are not executable.
    pub async fn has_vault(&self, workspace_id: &str, id: &str) -> Result<bool, CredentialError> {
        Ok(self
            .repository
            .get_vault(workspace_id, id)
            .await?
            .is_some_and(|vault| vault.accepts_child_mutation()))
    }

    /// The Workspace's Vaults that accept credential use, in repository order.
    pub async fn executable_vaults(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ManagedVault>, CredentialError> {
        let mut vaults = self.repository.list_vaults(workspace_id).await?;
        vaults.retain(ManagedVault::accepts_child_mutation);
        Ok(vaults)
    }

    /// The active credentials of one Vault in ascending id order, or `None`
    /// when the Vault does not accept credential use.
    pub async fn executable_credentials(
        &self,
        workspace_id: &str,
        vault_id: &str,
    ) -> Result<Option<Vec<ManagedVaultCredential>>, CredentialError> {
        if !self.has_vault(workspace_id, vault_id).await? {
            return Ok(None);
        }
        let mut credentials = self
            .repository
            .list_vault_credentials(workspace_id, vault_id)
            .await?;
        credentials.retain(|credential| credential.lifecycle.is_active());
        credentials.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(Some(credentials))
    }

    /// The neutral credential-domain row id of an active credential in
    /// `vault_id`: the resolver takes this `CredentialSourceId`, never the wire
    /// id.
    pub async fn credential_source_id(
        &self,
        workspace_id: &str,
        vault_id: &str,
        credential_id: &str,
    ) -> Option<CredentialSourceId> {
        self.repository
            .get_vault_credential(workspace_id, credential_id)
            .await
            .ok()
            .flatten()
            .filter(|credential| {
                credential.vault_id == vault_id && credential.lifecycle.is_active()
            })
            .map(|credential| credential.source_id)
    }

    /// The credential source row.
    pub async fn source(
        &self,
        source_id: &CredentialSourceId,
    ) -> Result<CredentialSource, CredentialError> {
        self.repository.get(source_id).await
    }

    /// The authoritative Managed child of a credential source.
    pub async fn credential_of_source(
        &self,
        workspace_id: &str,
        source_id: &CredentialSourceId,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError> {
        self.repository
            .get_vault_credential_by_source(workspace_id, source_id)
            .await
    }

    /// Open the active material of one source for an exact, caller-owned use
    /// (an MCP inspection the Vault owner admitted). Never returned to a wire.
    pub async fn open_material(
        &self,
        source: &CredentialSource,
    ) -> Result<RedactedString, CredentialError> {
        crate::materialize(source, self.secrets.as_ref()).await
    }

    // ---- Exact credential access ---------------------------------------------

    /// Compile one exact, secret-free execution pin for a previously selected
    /// credential source. The open deployment reads only the active revision;
    /// an installed hosted issuer may open that exact material solely to attach
    /// the existing recipient-bound envelope to the returned secret-free pin.
    /// `now_unix_ms` is sampled here; the request carries everything else.
    pub async fn exact_access<'r>(
        &self,
        source_id: &CredentialSourceId,
        request: impl FnOnce(u64) -> ExactCredentialAccessRequest<'r>,
    ) -> Result<(CredentialSource, CredentialAccess), CredentialError> {
        let now_unix_ms = self.clock_reading_ms()?;
        let source = self.repository.get(source_id).await?;
        let access = crate::publish_exact_credential_access(
            &source,
            request(now_unix_ms),
            self.material_delivery
                .as_ref()
                .map(|delivery| (self.secrets.as_ref(), delivery)),
        )
        .await?;
        Ok((source, access))
    }

    /// The exact MCP execution pin for `audience`, with the OAuth refresh
    /// instruction of the same revision attached when the credential holds
    /// one. `workspace_id` narrows admission at the exact row read.
    pub async fn mcp_access(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: Option<&str>,
        audience: String,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<CredentialAccess, CredentialError> {
        let (source, mut access) = self
            .exact_access(source_id, |now_unix_ms| ExactCredentialAccessRequest {
                workspace_id,
                target: Some(CredentialTarget::new(
                    CredentialPurpose::McpAuthorization,
                    audience,
                )),
                usage: bearer_usage(),
                policy: awaken_credential_contract::CredentialExecutionPolicy::self_hosted_mcp(),
                selected_holder,
                binding,
                now_unix_ms,
            })
            .await?;
        let revision = access.credential.revision;
        let refresh = self
            .repository
            .get_vault_credential_by_source(&source.workspace_id, source_id)
            .await?
            .and_then(|record| match record.auth {
                ManagedCredentialAuth::McpOauth { refresh, .. } => refresh,
                _ => None,
            });
        if let Some(refresh) = refresh {
            access = access.with_refresh(oauth_refresh_access(&source, revision, refresh)?);
        }
        Ok(access)
    }

    /// The exact refresh/reseal instruction of a credential's current
    /// revision, or `None` when the persisted row cannot compile one.
    pub async fn refresh_access(
        &self,
        source_id: &CredentialSourceId,
        config: ManagedMcpOauthRefresh,
    ) -> Option<(CredentialSourceId, CredentialRefreshAccess)> {
        let source = self.repository.get(source_id).await.ok()?;
        let revision = u64::try_from(source.version).ok()?;
        let access = oauth_refresh_access(&source, revision, config).ok()?;
        Some((source.id, access))
    }

    // ---- Consumer-neutral material ingress -----------------------------------

    /// Enter one described material idempotently under `source_id`.
    pub async fn enter_material(
        &self,
        source_id: CredentialSourceId,
        workspace_id: &str,
        descriptor: CredentialDescriptor,
        material: RedactedString,
    ) -> Result<MaterialEntry, String> {
        let entry = crate::repo::enter_credential_idempotent_described(
            source_id,
            CredentialCreateParams {
                workspace_id: workspace_id.to_string(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(material),
                oauth_command: None,
            },
            None,
            descriptor,
            self.secrets.as_ref(),
            self.repository.as_ref(),
            self.clock.as_ref(),
        )
        .await
        .map_err(|error| error.to_string())?;
        let revision = u64::try_from(entry.source.version)
            .map_err(|_| "credential revision exceeds the Session range".to_string())?;
        Ok(MaterialEntry {
            credential: CredentialRef {
                id: entry.source.id.0,
                revision,
            },
            created: entry.created,
        })
    }

    /// Rotate the material of `source_id` from exactly `expected_revision`;
    /// a replay of the completed rotation with the same material succeeds.
    pub async fn rotate_material(
        &self,
        source_id: &CredentialSourceId,
        expected_revision: u64,
        workspace_id: &str,
        expected_descriptor: &CredentialDescriptor,
        material: RedactedString,
    ) -> Result<u64, String> {
        let current = self
            .repository
            .get(source_id)
            .await
            .map_err(|error| error.to_string())?;
        if current.workspace_id != workspace_id
            || current.descriptor.as_ref() != Some(expected_descriptor)
        {
            return Err("credential binding is unavailable in this Workspace".into());
        }
        let expected_version = i64::try_from(expected_revision)
            .map_err(|_| "credential revision exceeds the Vault range".to_string())?;
        let completed_version = expected_version
            .checked_add(1)
            .ok_or_else(|| "credential revision exceeds the Vault range".to_string())?;
        if current.version == completed_version {
            if current.status != CredentialStatus::Active {
                return Err("credential is not active".into());
            }
            let sealed = crate::materialize(&current, self.secrets.as_ref())
                .await
                .map_err(|error| error.to_string())?;
            if sealed.expose_secret() != material.expose_secret() {
                return Err("credential revision changed before material rotation".into());
            }
            return u64::try_from(current.version)
                .map_err(|_| "credential revision exceeds the Session range".into());
        }
        if current.version != expected_version {
            return Err("credential revision changed before material rotation".into());
        }
        let rotated = rotate_credential_materials_exact(
            source_id,
            expected_version,
            CredentialMaterialPatch {
                primary: Some(material),
                auxiliary: BTreeMap::new(),
                descriptor: None,
            },
            self.secrets.as_ref(),
            self.repository.as_ref(),
            self.clock.as_ref(),
        )
        .await
        .map_err(|error| error.to_string())?;
        u64::try_from(rotated.version)
            .map_err(|_| "credential revision exceeds the Session range".into())
    }

    /// Retire the exact credential revision `credential`; a replay of the
    /// completed retirement succeeds.
    pub async fn retire_material(
        &self,
        credential: CredentialRef,
        workspace_id: &str,
    ) -> Result<(), String> {
        let source_id = CredentialSourceId(credential.id);
        let source = self
            .repository
            .get(&source_id)
            .await
            .map_err(|error| error.to_string())?;
        if source.workspace_id != workspace_id {
            return Err("credential binding is unavailable in this Workspace".into());
        }
        let expected = i64::try_from(credential.revision)
            .map_err(|_| "credential revision exceeds the Vault range".to_string())?;
        let completed = expected
            .checked_add(1)
            .ok_or_else(|| "credential revision exceeds the Vault range".to_string())?;
        if source.version == completed
            && source.status == CredentialStatus::Archived
            && source.material_ref.is_none()
            && source.auxiliary_material_refs.is_empty()
        {
            return Ok(());
        }
        revoke_credential_exact(
            &source_id,
            expected,
            CredentialRetirement::Archive,
            self.secrets.as_ref(),
            self.repository.as_ref(),
            self.clock.as_ref(),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }
}

/// The `Authorization: Bearer` usage every MCP credential is bound to.
fn bearer_usage() -> CredentialUsage {
    CredentialUsage::HttpHeader {
        name: "authorization".into(),
        scheme: Some("Bearer".into()),
    }
}

/// The descriptor of an opaque bearer authorizing the MCP endpoint
/// `audience`.
#[must_use]
pub fn mcp_bearer_descriptor(audience: &str) -> CredentialDescriptor {
    CredentialDescriptor::new(
        audience.to_owned(),
        CredentialMaterialDescriptor::secret(OPAQUE_SECRET_MATERIAL_TYPE),
        [CredentialTargetContract::new(
            CredentialTarget::new(CredentialPurpose::McpAuthorization, audience.to_owned()),
            bearer_usage(),
        )],
    )
}

/// A domain-separated, length-framed SHA-256 identity. Persisted Vault, source
/// and credential ids derive from it, so its framing never changes.
#[must_use]
pub fn identity_digest(domain: &str, parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain.len().to_be_bytes());
    digest.update(domain.as_bytes());
    for part in parts {
        digest.update(part.len().to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn application_mcp_vault_id(workspace_id: &str, authority_id: &str) -> String {
    format!(
        "vlt_app_{}",
        identity_digest("application-mcp-vault-v1", &[workspace_id, authority_id])
    )
}

fn application_mcp_source_id(vault_id: &str, target_fingerprint: &str) -> CredentialSourceId {
    CredentialSourceId(format!(
        "cred:app-mcp:{vault_id}:{}",
        identity_digest("application-mcp-target-v1", &[target_fingerprint])
    ))
}

/// Apply a metadata patch in place: a `Some(v)` upserts the key, a `None` (JSON
/// `null`) deletes it, and any key absent from the patch is preserved.
pub fn apply_metadata_patch(
    target: &mut BTreeMap<String, String>,
    patch: BTreeMap<String, Option<String>>,
) {
    for (key, value) in patch {
        match value {
            Some(v) => {
                target.insert(key, v);
            }
            None => {
                target.remove(&key);
            }
        }
    }
}

/// Compile the exact OAuth refresh/reseal instruction for one credential
/// revision from its persisted refresh configuration and sealed material refs.
fn oauth_refresh_access(
    source: &CredentialSource,
    revision: u64,
    refresh: ManagedMcpOauthRefresh,
) -> Result<CredentialRefreshAccess, CredentialError> {
    let source_id = &source.id;
    let access_token_ref = source
        .material_ref
        .as_ref()
        .ok_or_else(|| CredentialError::MissingMaterialRef(source_id.0.clone()))?
        .0
        .clone();
    let refresh_token_ref = source
        .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
        .ok_or_else(|| {
            CredentialError::MissingMaterialRef(format!(
                "{}:{OAUTH_REFRESH_TOKEN_SLOT}",
                source_id.0
            ))
        })?
        .0
        .clone();
    let client_secret_ref = match refresh.token_endpoint_auth {
        TokenEndpointAuth::None => None,
        TokenEndpointAuth::ClientSecretBasic | TokenEndpointAuth::ClientSecretPost => Some(
            source
                .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
                .ok_or_else(|| {
                    CredentialError::MissingMaterialRef(format!(
                        "{}:{OAUTH_CLIENT_SECRET_SLOT}",
                        source_id.0
                    ))
                })?
                .0
                .clone(),
        ),
    };
    Ok(CredentialRefreshAccess::new(
        revision,
        refresh.token_endpoint,
        refresh.client_id,
        refresh.token_endpoint_auth,
        client_secret_ref,
        refresh_token_ref,
        access_token_ref,
        refresh.scope,
        refresh.resource,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ManagedVaultRepo;
    use crate::repo::InMemoryCredentialRepo;

    #[tokio::test]
    async fn stale_delete_writer_resumes_the_durable_winner() {
        let repository = Arc::new(InMemoryCredentialRepo::new());
        let application = VaultApplication::new(
            Arc::new(crate::InMemorySecretStore::new()),
            repository.clone(),
            awaken_time::system_clock(),
        );
        let current = ManagedVault {
            id: "vault-delete-race".into(),
            workspace_id: "workspace".into(),
            display_name: "Vault".into(),
            metadata: BTreeMap::new(),
            created_at: "2026-09-17T00:00:00Z".into(),
            updated_at: "2026-09-17T00:00:00Z".into(),
            archived_at: None,
            deletion: None,
            revision: 1,
        };
        repository
            .insert_vault("workspace", current.clone())
            .await
            .unwrap();
        let (requested, changed) =
            request_managed_vault_deletion(&current, "2026-09-17T08:00:00Z".to_owned()).unwrap();
        assert!(changed);

        // Deterministically reproduce the interleaving of two DELETE commands:
        // both read `current`, then the first publishes `requested` before the
        // second attempts the same stale CAS.
        repository
            .replace_vault("workspace", current.revision, requested.clone())
            .await
            .unwrap();
        let resumed = application
            .persist_vault_deletion_request("workspace", &current, requested.clone(), changed)
            .await
            .expect("the stale loser must resume the durable delete request");

        assert_eq!(resumed, requested);
        assert!(resumed.deletion_requested());
    }

    #[test]
    fn identity_digest_framing_is_pinned() {
        // Persisted ids derive from this digest: a change of framing would
        // orphan every application Vault, source and idempotent credential.
        assert_eq!(
            identity_digest("application-mcp-vault-v1", &["ws", "app"]),
            {
                let mut digest = Sha256::new();
                for part in ["application-mcp-vault-v1", "ws", "app"] {
                    digest.update(part.len().to_be_bytes());
                    digest.update(part.as_bytes());
                }
                format!("{:x}", digest.finalize())
            }
        );
        assert_ne!(
            identity_digest("d", &["ab", "c"]),
            identity_digest("d", &["a", "bc"]),
            "length framing separates part boundaries"
        );
    }

    #[test]
    fn metadata_patch_upserts_deletes_and_preserves() {
        let mut metadata = BTreeMap::from([
            ("keep".to_string(), "1".to_string()),
            ("drop".to_string(), "2".to_string()),
            ("change".to_string(), "3".to_string()),
        ]);
        apply_metadata_patch(
            &mut metadata,
            BTreeMap::from([
                ("drop".to_string(), None),
                ("change".to_string(), Some("4".to_string())),
                ("add".to_string(), Some("5".to_string())),
            ]),
        );
        assert_eq!(
            metadata,
            BTreeMap::from([
                ("add".to_string(), "5".to_string()),
                ("change".to_string(), "4".to_string()),
                ("keep".to_string(), "1".to_string()),
            ])
        );
    }
}
