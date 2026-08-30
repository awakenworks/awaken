//! Executable terminal effects, exact Memory reconciliation, and completion errors.

use super::state::SessionCleanupState;
use super::*;

pub(super) fn complete_cleanup(
    cleanup: &mut SessionCleanupOperation,
    session_id: &str,
    receipts: &[VerifiedSessionCleanupReceipt],
    repository_publication_outcome: Option<VerifiedRepositoryPublicationOutcome>,
) -> Result<bool, SessionCleanupError> {
    let SessionCleanupState::Requested {
        effect_id,
        thread_ids,
        delegation_watermark,
        runtime_commit_cursor,
        ..
    } = cleanup.state()
    else {
        return if cleanup.is_completed() {
            Ok(false)
        } else {
            Err(SessionCleanupError::NotRequested)
        };
    };
    if receipts.is_empty() {
        return Err(SessionCleanupError::MissingReceipt);
    }
    let mut evidence = receipts.to_vec();
    evidence.sort_by(|left, right| left.thread_id().cmp(right.thread_id()));
    for pair in evidence.windows(2) {
        if pair[0].thread_id() == pair[1].thread_id() {
            return Err(SessionCleanupError::DuplicateThread(
                pair[0].thread_id().to_string(),
            ));
        }
    }
    let evidenced_threads = evidence
        .iter()
        .map(|receipt| receipt.thread_id().to_string())
        .collect::<BTreeSet<_>>();
    if !evidenced_threads.contains(session_id) {
        return Err(SessionCleanupError::MissingRootReceipt);
    }
    if evidenced_threads != *thread_ids {
        return Err(SessionCleanupError::MissingReceipt);
    }
    for receipt in &evidence {
        let expected = SessionCleanupCommand::new(session_id, receipt.thread_id(), effect_id);
        if receipt.command() != &expected {
            return Err(SessionCleanupError::ReceiptMismatch);
        }
    }
    let cleanup_evidence = evidence
        .iter()
        .map(|receipt| {
            (
                receipt.thread_id(),
                receipt.effect_id(),
                receipt.receipt_fingerprint(),
            )
        })
        .collect::<Vec<_>>();
    let receipt_fingerprint = match repository_publication_outcome {
        Some(VerifiedRepositoryPublicationOutcome::Published(publication_receipt)) => {
            crate::stable_fingerprint(&(
                "session-terminal-cleanup-receipt-v2",
                effect_id.as_str(),
                *delegation_watermark,
                cleanup_evidence,
                publication_receipt.receipt_fingerprint.as_str(),
            ))
        }
        Some(VerifiedRepositoryPublicationOutcome::Rejected(publication_rejection)) => {
            crate::stable_fingerprint(&(
                "session-terminal-cleanup-rejection-v1",
                effect_id.as_str(),
                *delegation_watermark,
                cleanup_evidence,
                publication_rejection.rejection_fingerprint.as_str(),
            ))
        }
        None => crate::stable_fingerprint(&(
            "session-terminal-cleanup-receipt-v1",
            effect_id.as_str(),
            *delegation_watermark,
            cleanup_evidence,
        )),
    };
    let next = SessionCleanupOperation::from_state(SessionCleanupState::Completed {
        effect_id: effect_id.clone(),
        thread_ids: thread_ids.clone(),
        delegation_watermark: *delegation_watermark,
        runtime_commit_cursor: *runtime_commit_cursor,
        receipt_fingerprint,
    });
    if !cleanup.advance_to(next) {
        return Err(SessionCleanupError::InvalidPhaseAdvance);
    }
    Ok(true)
}

/// Stable, per-thread cleanup command derived from the root operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCleanupCommand {
    pub session_id: String,
    pub thread_id: String,
    pub effect_id: String,
    /// Exact already-created restore target that terminal cleanup must prepare
    /// and later dispose. It is valid only on the root command and never asks
    /// Runtime to restore a hibernated Environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_target: Option<crate::SandboxRestoreRequest>,
}

impl SessionCleanupCommand {
    #[must_use]
    pub fn new(session_id: &str, thread_id: &str, root_effect_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            thread_id: thread_id.to_string(),
            effect_id: crate::stable_fingerprint(&(
                "session-terminal-cleanup-thread-v1",
                session_id,
                thread_id,
                root_effect_id,
            )),
            restore_target: None,
        }
    }

    pub fn with_restore_target(
        mut self,
        request: crate::SandboxRestoreRequest,
    ) -> Result<Self, SessionCleanupError> {
        if self.thread_id != self.session_id || request.session_id != self.session_id {
            return Err(SessionCleanupError::ReceiptMismatch);
        }
        match self.restore_target.as_ref() {
            Some(existing) if existing != &request => {
                return Err(SessionCleanupError::ReceiptMismatch);
            }
            _ => self.restore_target = Some(request),
        }
        Ok(self)
    }

    #[must_use]
    pub(crate) fn without_restore_target(&self) -> Self {
        let mut command = self.clone();
        command.restore_target = None;
        command
    }
}

/// One exact terminal-cleanup preparation together with the durable
/// realization generation allowed to execute it.
///
/// This is a transport value, not another operation or queue: `command` is
/// always re-derived from [`SessionCleanupOperation`] and `lease` is the
/// aggregate's existing realization fence. Runtime/provider adapters carry the
/// pair unchanged to their final physical-effect boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTerminalCleanupEffect {
    pub command: SessionCleanupCommand,
    pub lease: crate::SessionRealizationLease,
}

/// The one root-owned physical disposal together with the exact durable
/// realization generation allowed to execute it. This is projected only from
/// the operation's private `Disposing` wire state, after every source-dependent
/// preparation receipt is durable in the Session root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTerminalCleanupDisposalEffect {
    pub command: SessionCleanupDisposalCommand,
    pub lease: crate::SessionRealizationLease,
}

impl SessionTerminalCleanupDisposalEffect {
    #[must_use]
    pub fn new(
        command: SessionCleanupDisposalCommand,
        lease: crate::SessionRealizationLease,
    ) -> Self {
        Self { command, lease }
    }

    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.command.effect_id
    }

    /// Project the exact durable preparation predecessor and current live
    /// successor into the one provider-neutral physical-disposal authority.
    /// Adapters must carry this value unchanged; rebuilding either fence at the
    /// provider boundary would create a second failover rule.
    pub fn sandbox_disposal_authorization(
        &self,
    ) -> Result<
        awaken_provisioning_contract::SandboxDisposalAuthorization,
        awaken_provisioning_contract::SandboxError,
    > {
        self.sandbox_disposal_authorization_for_current_generation(&self.lease)
    }

    /// Lower a same-generation monotonic renewal into the provider successor
    /// fence while retaining the aggregate's immutable durable predecessor.
    /// The caller still owns current-liveness validation at its effect boundary.
    pub fn sandbox_disposal_authorization_for_current_generation(
        &self,
        current_lease: &crate::SessionRealizationLease,
    ) -> Result<
        awaken_provisioning_contract::SandboxDisposalAuthorization,
        awaken_provisioning_contract::SandboxError,
    > {
        if !crate::realization_lease_generation_authorizes(current_lease, &self.lease) {
            return Err(awaken_provisioning_contract::SandboxError::new(
                "terminal disposal realization generation was replaced",
            ));
        }
        let current = current_lease.sandbox_effect_fence(self.command.effect_id.clone())?;
        self.command.provider_disposal.authorize(current)
    }
}

impl SessionTerminalCleanupEffect {
    #[must_use]
    pub fn new(command: SessionCleanupCommand, lease: crate::SessionRealizationLease) -> Self {
        Self { command, lease }
    }

    /// Stable provider operation identity. The lease epoch remains a separate
    /// monotonic fence so an exact response-loss retry reuses the operation,
    /// while a replacement realization cannot impersonate its predecessor.
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.command.effect_id
    }

    /// Lower this exact work assertion into the provider-neutral fence. The
    /// provider preparation result remains a separate receipt field and must
    /// never be reconstructed from a later lease.
    pub fn sandbox_effect_fence(
        &self,
    ) -> Result<
        awaken_provisioning_contract::SandboxEffectFence,
        awaken_provisioning_contract::SandboxError,
    > {
        self.lease
            .sandbox_effect_fence(self.command.effect_id.clone())
    }
}

/// Exact, untrusted terminal Memory reconciliation request projected from one
/// frozen Session input and the durable copy bases in its current Sandbox
/// binding. Workspace ownership is deliberately absent: only the Session root
/// may add that fact by returning [`SessionTerminalMemoryTarget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTerminalMemoryIntent {
    binding_id: awaken_resource_contract::BindingId,
    config_version: awaken_resource_contract::ConfigVersion,
    access: awaken_resource_contract::ResourceAccess,
    materialization: awaken_provisioning_contract::MemoryMaterializationEvidence,
    effect: SessionTerminalCleanupEffect,
}

impl SessionTerminalMemoryIntent {
    /// Project one exact writable Memory input and its durable original heads.
    /// No path, store, access, or effect identity is normalized or inferred.
    pub fn try_new(
        input: &crate::ResolvedInput,
        materialization: &awaken_provisioning_contract::MemoryMaterializationEvidence,
        effect: &SessionTerminalCleanupEffect,
    ) -> Result<Self, SessionMemoryReconciliationError> {
        let crate::ResolvedInputSource::MemoryStore {
            memory_store_id,
            config,
        } = &input.source
        else {
            return Err(SessionMemoryReconciliationError::InvalidIntent(
                "terminal Memory reconciliation requires a Memory input".into(),
            ));
        };
        if memory_store_id.as_str() != materialization.store_id
            || input.mount_path != materialization.mount_path
        {
            return Err(SessionMemoryReconciliationError::ResourceMismatch);
        }
        Self::try_from_untrusted_parts(
            input.binding_id.clone(),
            config.version,
            input.access,
            materialization.clone(),
            effect.clone(),
        )
    }

    /// Rebuild the neutral value after strict transport decoding. This performs
    /// structural validation only; callers must still obtain a root-authorized
    /// [`SessionTerminalMemoryTarget`] before any Memory I/O.
    pub fn try_from_untrusted_parts(
        binding_id: awaken_resource_contract::BindingId,
        config_version: awaken_resource_contract::ConfigVersion,
        access: awaken_resource_contract::ResourceAccess,
        materialization: awaken_provisioning_contract::MemoryMaterializationEvidence,
        effect: SessionTerminalCleanupEffect,
    ) -> Result<Self, SessionMemoryReconciliationError> {
        if binding_id.as_str().trim().is_empty() {
            return Err(SessionMemoryReconciliationError::InvalidIntent(
                "terminal Memory binding id is empty".into(),
            ));
        }
        if access != awaken_resource_contract::ResourceAccess::ReadWrite {
            return Err(SessionMemoryReconciliationError::InvalidIntent(
                "terminal Memory reconciliation requires read-write access".into(),
            ));
        }
        if effect.command.thread_id != effect.command.session_id {
            return Err(SessionMemoryReconciliationError::InvalidIntent(
                "terminal Memory reconciliation is root-Session only".into(),
            ));
        }
        materialization
            .validate()
            .map_err(|error| SessionMemoryReconciliationError::InvalidIntent(error.to_string()))?;
        Ok(Self {
            binding_id,
            config_version,
            access,
            materialization,
            effect,
        })
    }

    #[must_use]
    pub fn binding_id(&self) -> &awaken_resource_contract::BindingId {
        &self.binding_id
    }

    #[must_use]
    pub fn memory_store_id(&self) -> &str {
        &self.materialization.store_id
    }

    #[must_use]
    pub const fn config_version(&self) -> awaken_resource_contract::ConfigVersion {
        self.config_version
    }

    #[must_use]
    pub fn mount_path(&self) -> &str {
        &self.materialization.mount_path
    }

    #[must_use]
    pub const fn access(&self) -> awaken_resource_contract::ResourceAccess {
        self.access
    }

    #[must_use]
    pub fn materialization(&self) -> &awaken_provisioning_contract::MemoryMaterializationEvidence {
        &self.materialization
    }

    #[must_use]
    pub fn effect(&self) -> &SessionTerminalCleanupEffect {
        &self.effect
    }

    /// Exact frozen-input comparison used by the Session aggregate. Store id
    /// alone is insufficient because one store may be mounted more than once.
    #[must_use]
    pub fn matches_input(&self, input: &crate::ResolvedInput) -> bool {
        input.binding_id == self.binding_id
            && input.mount_path == self.materialization.mount_path
            && input.access == self.access
            && matches!(
                &input.source,
                crate::ResolvedInputSource::MemoryStore {
                    memory_store_id,
                    config,
                } if memory_store_id.as_str() == self.materialization.store_id
                    && config.version == self.config_version
            )
    }
}

/// Canonical terminal Memory target returned by the Session root. It wraps the
/// exact untrusted intent instead of copying its identities, and adds only the
/// authoritative Workspace partition read from the root repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTerminalMemoryTarget {
    workspace_id: String,
    intent: SessionTerminalMemoryIntent,
}

impl SessionTerminalMemoryTarget {
    /// Construct only after [`crate::PersistedSession`] admitted `intent`
    /// against its cleanup operation, active input, and current Sandbox handle.
    pub fn from_authorized_root(
        workspace_id: impl Into<String>,
        intent: SessionTerminalMemoryIntent,
    ) -> Result<Self, SessionMemoryReconciliationError> {
        let workspace_id = workspace_id.into();
        if workspace_id.trim().is_empty() {
            return Err(SessionMemoryReconciliationError::InvalidIntent(
                "terminal Memory target has no canonical Workspace".into(),
            ));
        }
        Ok(Self {
            workspace_id,
            intent,
        })
    }

    #[must_use]
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    #[must_use]
    pub fn intent(&self) -> &SessionTerminalMemoryIntent {
        &self.intent
    }

    #[must_use]
    pub fn memory_store_id(&self) -> &str {
        self.intent.memory_store_id()
    }

    #[must_use]
    pub const fn config_version(&self) -> awaken_resource_contract::ConfigVersion {
        self.intent.config_version()
    }

    #[must_use]
    pub const fn access(&self) -> awaken_resource_contract::ResourceAccess {
        self.intent.access()
    }
}

/// Pure adapter retained at the Runtime boundary so callers do not duplicate
/// the exact input/evidence join.
pub fn terminal_memory_reconciliation_intent(
    input: &crate::ResolvedInput,
    materialization: &awaken_provisioning_contract::MemoryMaterializationEvidence,
    effect: &SessionTerminalCleanupEffect,
) -> Result<SessionTerminalMemoryIntent, SessionMemoryReconciliationError> {
    SessionTerminalMemoryIntent::try_new(input, materialization, effect)
}

/// Project all copy-backed Memory reconciliations from one frozen input set and
/// the current Sandbox handle's canonical evidence. Evidence identifies a
/// mount, not a binding, so every item must resolve to exactly one complete
/// frozen input before the existing single-item constructor is invoked.
/// Read-only copies and write-through mounts require no terminal CAS and are
/// deliberately omitted.
pub fn terminal_memory_reconciliation_intents(
    inputs: &[crate::ResolvedInput],
    materializations: &[awaken_provisioning_contract::MemoryMaterializationEvidence],
    effect: &SessionTerminalCleanupEffect,
) -> Result<Vec<SessionTerminalMemoryIntent>, SessionMemoryReconciliationError> {
    join_memory_materializations(inputs, materializations)?
        .into_iter()
        .filter(|joined| joined.input.access == awaken_resource_contract::ResourceAccess::ReadWrite)
        .map(|joined| {
            SessionTerminalMemoryIntent::try_new(joined.input, joined.materialization, effect)
        })
        .collect()
}

/// Validate the complete copy evidence before a checkpoint source is released.
/// A read-only copy has no write-back obligation; a writable copy is historical
/// or drifted because checkpoint-and-release projects writable Memory as
/// write-through. The caller must reject that row before provider mutation.
pub fn validate_continuation_memory_reconciliation(
    inputs: &[crate::ResolvedInput],
    materializations: &[awaken_provisioning_contract::MemoryMaterializationEvidence],
) -> Result<(), SessionMemoryReconciliationError> {
    let joined = join_memory_materializations(inputs, materializations)?;
    if joined
        .iter()
        .any(|joined| joined.input.access == awaken_resource_contract::ResourceAccess::ReadWrite)
    {
        return Err(SessionMemoryReconciliationError::WritableCopyRequiresTerminalReconciliation);
    }
    Ok(())
}

/// Interpret the current Sandbox handle's optional Memory evidence once for
/// every continuation boundary. `None` is legacy/unknown evidence and is not
/// interchangeable with an explicit empty current set: writable Memory cannot
/// be proved write-through in that shape. A present slice always flows through
/// the canonical input/evidence join above and is returned unchanged so the
/// Runtime can acknowledge that exact complete set after validation.
pub fn validate_continuation_memory_materializations<'evidence>(
    inputs: &[crate::ResolvedInput],
    materializations: Option<
        &'evidence [awaken_provisioning_contract::MemoryMaterializationEvidence],
    >,
) -> Result<
    Option<&'evidence [awaken_provisioning_contract::MemoryMaterializationEvidence]>,
    SessionMemoryReconciliationError,
> {
    let Some(materializations) =
        validate_optional_memory_materializations(inputs, materializations)?
    else {
        return Ok(None);
    };
    validate_continuation_memory_reconciliation(inputs, materializations)?;
    Ok(Some(materializations))
}

/// Interpret optional handle evidence for terminal cleanup without applying the
/// continuation-only RW-Copy rejection. A present slice may produce writable
/// reconciliation intents; legacy absence with writable Memory is ambiguous
/// and therefore fails before Artifact or provider effects.
pub fn terminal_memory_reconciliation_intents_from_materializations<'evidence>(
    inputs: &[crate::ResolvedInput],
    materializations: Option<
        &'evidence [awaken_provisioning_contract::MemoryMaterializationEvidence],
    >,
    effect: &SessionTerminalCleanupEffect,
) -> Result<
    (
        Vec<SessionTerminalMemoryIntent>,
        Option<&'evidence [awaken_provisioning_contract::MemoryMaterializationEvidence]>,
    ),
    SessionMemoryReconciliationError,
> {
    let Some(materializations) =
        validate_optional_memory_materializations(inputs, materializations)?
    else {
        return Ok((Vec::new(), None));
    };
    let intents = terminal_memory_reconciliation_intents(inputs, materializations, effect)?;
    Ok((intents, Some(materializations)))
}

fn validate_optional_memory_materializations<'evidence>(
    inputs: &[crate::ResolvedInput],
    materializations: Option<
        &'evidence [awaken_provisioning_contract::MemoryMaterializationEvidence],
    >,
) -> Result<
    Option<&'evidence [awaken_provisioning_contract::MemoryMaterializationEvidence]>,
    SessionMemoryReconciliationError,
> {
    if materializations.is_none()
        && inputs.iter().any(|input| {
            matches!(
                &input.source,
                crate::ResolvedInputSource::MemoryStore { .. }
            ) && input.access == awaken_resource_contract::ResourceAccess::ReadWrite
        })
    {
        return Err(SessionMemoryReconciliationError::WritableCopyRequiresTerminalReconciliation);
    }
    Ok(materializations)
}

struct JoinedMemoryMaterialization<'input, 'materialization> {
    input: &'input crate::ResolvedInput,
    materialization: &'materialization awaken_provisioning_contract::MemoryMaterializationEvidence,
}

/// The sole input/evidence correlation owner. Terminal cleanup consumes the
/// writable rows as CAS intents; continuation accepts only read-only rows.
fn join_memory_materializations<'input, 'materialization>(
    inputs: &'input [crate::ResolvedInput],
    materializations: &'materialization [
        awaken_provisioning_contract::MemoryMaterializationEvidence
    ],
) -> Result<
    Vec<JoinedMemoryMaterialization<'input, 'materialization>>,
    SessionMemoryReconciliationError,
> {
    awaken_provisioning_contract::MemoryMaterializationEvidence::validate_all(materializations)
        .map_err(|error| SessionMemoryReconciliationError::InvalidIntent(error.to_string()))?;
    let mut joined = Vec::with_capacity(materializations.len());
    for materialization in materializations {
        let mut at_mount = inputs
            .iter()
            .filter(|input| input.mount_path == materialization.mount_path);
        let input = at_mount
            .next()
            .ok_or(SessionMemoryReconciliationError::ResourceMismatch)?;
        if at_mount.next().is_some() {
            return Err(SessionMemoryReconciliationError::ResourceMismatch);
        }
        let crate::ResolvedInputSource::MemoryStore {
            memory_store_id, ..
        } = &input.source
        else {
            return Err(SessionMemoryReconciliationError::ResourceMismatch);
        };
        if memory_store_id.as_str() != materialization.store_id {
            return Err(SessionMemoryReconciliationError::ResourceMismatch);
        }
        joined.push(JoinedMemoryMaterialization {
            input,
            materialization,
        });
    }
    Ok(joined)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionMemoryReconciliationError {
    #[error("invalid Memory reconciliation evidence or intent: {0}")]
    InvalidIntent(String),
    #[error("Memory reconciliation does not match the frozen Session input")]
    ResourceMismatch,
    #[error("Memory reconciliation does not match the current Sandbox binding")]
    EnvironmentMismatch,
    #[error("writable copy-backed Memory requires root-terminal reconciliation")]
    WritableCopyRequiresTerminalReconciliation,
    #[error(transparent)]
    Cleanup(#[from] SessionCleanupError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionCleanupError {
    #[error("Session terminal cleanup was not requested")]
    NotRequested,
    #[error("Session terminal cleanup has no Runtime receipt")]
    MissingReceipt,
    #[error("Session terminal cleanup has no root-thread receipt")]
    MissingRootReceipt,
    #[error("Session terminal cleanup contains duplicate thread {0}")]
    DuplicateThread(String),
    #[error("Session terminal cleanup receipt does not match its exact intent")]
    ReceiptMismatch,
    #[error("Session terminal cleanup preparation does not match its exact command")]
    PreparationReceiptMismatch,
    #[error("Session Repository preparation does not match its exact retained plan")]
    RepositoryPreparationReceiptMismatch,
    #[error("Session terminal provider disposal preparation does not match Environment truth")]
    ProviderDisposalPreparationMismatch,
    #[error("Session terminal cleanup preparation is not ready for physical disposal")]
    DisposalNotReady,
    #[error("Session terminal cleanup disposal does not match its exact command")]
    DisposalReceiptMismatch,
    #[error("invalid Session Repository publication intent: {0}")]
    InvalidRepositoryPublicationIntent(String),
    #[error("Session Repository publication was not requested")]
    RepositoryPublicationNotRequested,
    #[error("Session Repository publication is not ready before child cleanup completes")]
    RepositoryPublicationNotReady,
    #[error("Session terminal cleanup has no Repository publication receipt")]
    MissingRepositoryPublicationReceipt,
    #[error("Session Repository publication receipt does not match its exact command")]
    RepositoryPublicationReceiptMismatch,
    #[error("Session terminal cleanup has no Repository publication outcome")]
    MissingRepositoryPublicationOutcome,
    #[error("Session Repository publication rejection does not match its exact command")]
    RepositoryPublicationRejectionMismatch,
    #[error("Session Repository publication has conflicting terminal outcomes")]
    RepositoryPublicationOutcomeMismatch,
    #[error("Session Repository publication intent is already frozen to another value")]
    FrozenRepositoryPublicationMismatch,
    #[error("Session cleanup operation does not match its Session identity")]
    OperationMismatch,
    #[error("Session terminal cleanup realization generation is stale")]
    RealizationMismatch,
    #[error("Session terminal cleanup command is not currently pending")]
    CommandNotPending,
    #[error("Session terminal cleanup targets were already frozen at a different watermark")]
    FrozenTargetsMismatch,
    #[error("Session cleanup operation attempted an invalid phase advance")]
    InvalidPhaseAdvance,
}

pub(super) fn cleanup_effect_id(session_id: &str) -> String {
    crate::stable_fingerprint(&("session-terminal-cleanup-v1", session_id))
}
