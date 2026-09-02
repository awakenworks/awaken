//! Durable preparation and single physical-disposal subphases.
//!
//! Both subphases wrap the unchanged private legacy Requested wire. That keeps
//! historical bytes exact, while an older reader fails on the new outer state
//! instead of treating preparation as historical physical-completion evidence.

use super::completion::canonical_thread_artifact_receipt_fingerprint;
use super::state::SessionCleanupState;
use super::*;
use awaken_resource_contract::ArtifactPublicationReceipt;
use serde::{Deserialize, Deserializer, Serialize};

/// Aggregate-owned proof that the canonical Session Repository retirement
/// plan completed through the existing Resource/Vault participant authority.
/// Per-target retry state remains solely in
/// `SessionResourceState::repository_retirements`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCleanupRepositoryPreparation {
    session_id: String,
    workspace_id: String,
    cleanup_effect_id: String,
    plan_fingerprint: String,
    receipt_fingerprint: String,
}

impl SessionCleanupRepositoryPreparation {
    pub fn new(
        session_id: &str,
        workspace_id: &str,
        resources: &crate::SessionResourceState,
    ) -> Result<Self, SessionCleanupError> {
        if session_id.trim().is_empty() || workspace_id.trim().is_empty() {
            return Err(SessionCleanupError::RepositoryPreparationReceiptMismatch);
        }
        let cleanup_effect_id = cleanup_effect_id(session_id);
        let plan = resources.terminal_repository_retirement_plan();
        let plan_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-repository-preparation-plan-v1",
            session_id,
            workspace_id,
            &plan,
        ));
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-repository-preparation-receipt-v1",
            session_id,
            workspace_id,
            cleanup_effect_id.as_str(),
            plan_fingerprint.as_str(),
        ));
        Ok(Self {
            session_id: session_id.to_string(),
            workspace_id: workspace_id.to_string(),
            cleanup_effect_id,
            plan_fingerprint,
            receipt_fingerprint,
        })
    }

    fn verify_identity(
        &self,
        session_id: &str,
        effect_id: &str,
    ) -> Result<(), SessionCleanupError> {
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-repository-preparation-receipt-v1",
            self.session_id.as_str(),
            self.workspace_id.as_str(),
            self.cleanup_effect_id.as_str(),
            self.plan_fingerprint.as_str(),
        ));
        if self.session_id != session_id
            || self.cleanup_effect_id != effect_id
            || self.workspace_id.trim().is_empty()
            || self.plan_fingerprint.trim().is_empty()
            || self.receipt_fingerprint != receipt_fingerprint
        {
            return Err(SessionCleanupError::RepositoryPreparationReceiptMismatch);
        }
        Ok(())
    }

    pub(crate) fn verify_for_resources(
        &self,
        session_id: &str,
        workspace_id: &str,
        resources: &crate::SessionResourceState,
    ) -> Result<(), SessionCleanupError> {
        if self != &Self::new(session_id, workspace_id, resources)? {
            return Err(SessionCleanupError::RepositoryPreparationReceiptMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    #[must_use]
    pub fn receipt_fingerprint(&self) -> &str {
        &self.receipt_fingerprint
    }
}

/// Untrusted Runtime evidence that every source-dependent effect for one
/// frozen thread has become durable, without claiming physical disposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCleanupPreparation {
    /// Exact preparation command and the realization lease under which the
    /// Runtime completed its source-dependent effects. Keeping the pair intact
    /// is required for a later aggregate-authorized disposer to take over the
    /// provider participant without reconstructing its predecessor fence from
    /// a newer lease.
    pub effect: SessionTerminalCleanupEffect,
    /// Exact durable fence returned by the provider preparation boundary. It
    /// is deliberately distinct from `effect`: source work may begin under A
    /// and cross the provider boundary under C, while a response-loss retry may
    /// assert D and recover the already-durable C.
    provider_prepared_effect_fence: awaken_provisioning_contract::SandboxEffectFence,
    pub artifact_receipts: Vec<ArtifactPublicationReceipt>,
    pub receipt_fingerprint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionCleanupPreparationWire {
    effect: SessionTerminalCleanupEffect,
    #[serde(default)]
    provider_prepared_effect_fence: Option<awaken_provisioning_contract::SandboxEffectFence>,
    artifact_receipts: Vec<ArtifactPublicationReceipt>,
    receipt_fingerprint: String,
}

impl<'de> Deserialize<'de> for SessionCleanupPreparation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SessionCleanupPreparationWire::deserialize(deserializer)?;
        let asserted_effect_fence = wire
            .effect
            .sandbox_effect_fence()
            .map_err(serde::de::Error::custom)?;
        let preparation = Self {
            effect: wire.effect,
            provider_prepared_effect_fence: wire
                .provider_prepared_effect_fence
                .unwrap_or(asserted_effect_fence),
            artifact_receipts: wire.artifact_receipts,
            receipt_fingerprint: wire.receipt_fingerprint,
        };
        preparation
            .verify(&preparation.effect)
            .map_err(serde::de::Error::custom)?;
        Ok(preparation)
    }
}

impl SessionCleanupPreparation {
    pub fn try_new(
        effect: &SessionTerminalCleanupEffect,
        provider_prepared_effect_fence: awaken_provisioning_contract::SandboxEffectFence,
        mut artifact_receipts: Vec<ArtifactPublicationReceipt>,
    ) -> Result<Self, SessionCleanupError> {
        let asserted_effect_fence = effect
            .sandbox_effect_fence()
            .map_err(|_| SessionCleanupError::PreparationReceiptMismatch)?;
        provider_prepared_effect_fence
            .validate_identity()
            .map_err(|_| SessionCleanupError::PreparationReceiptMismatch)?;
        if !asserted_effect_fence.same_realization_lease(&provider_prepared_effect_fence)
            || !(asserted_effect_fence.authorizes_effect_successor(&provider_prepared_effect_fence)
                || provider_prepared_effect_fence
                    .authorizes_effect_successor(&asserted_effect_fence))
        {
            return Err(SessionCleanupError::PreparationReceiptMismatch);
        }
        let (receipts_are_unique, artifact_fingerprint) =
            canonical_thread_artifact_receipt_fingerprint(
                "session-terminal-cleanup-thread-preparation-v1",
                &effect.command,
                &mut artifact_receipts,
            );
        if !receipts_are_unique {
            return Err(SessionCleanupError::PreparationReceiptMismatch);
        }
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-thread-preparation-effect-v2",
            artifact_fingerprint.as_str(),
            effect.lease.owner.as_str(),
            effect.lease.runtime_incarnation.as_str(),
            effect.lease.epoch,
            effect.lease.expires_at_unix_ms,
            &provider_prepared_effect_fence,
        ));
        Ok(Self {
            effect: effect.clone(),
            provider_prepared_effect_fence,
            artifact_receipts,
            receipt_fingerprint,
        })
    }

    #[must_use]
    pub const fn provider_prepared_effect_fence(
        &self,
    ) -> &awaken_provisioning_contract::SandboxEffectFence {
        &self.provider_prepared_effect_fence
    }

    fn verify(&self, effect: &SessionTerminalCleanupEffect) -> Result<(), SessionCleanupError> {
        let asserted_effect_fence = effect
            .sandbox_effect_fence()
            .map_err(|_| SessionCleanupError::PreparationReceiptMismatch)?;
        self.provider_prepared_effect_fence
            .validate_identity()
            .map_err(|_| SessionCleanupError::PreparationReceiptMismatch)?;
        if !asserted_effect_fence.same_realization_lease(&self.provider_prepared_effect_fence)
            || !(asserted_effect_fence
                .authorizes_effect_successor(&self.provider_prepared_effect_fence)
                || self
                    .provider_prepared_effect_fence
                    .authorizes_effect_successor(&asserted_effect_fence))
        {
            return Err(SessionCleanupError::PreparationReceiptMismatch);
        }
        let mut canonical_receipts = self.artifact_receipts.clone();
        let (receipts_are_unique, artifact_fingerprint) =
            canonical_thread_artifact_receipt_fingerprint(
                "session-terminal-cleanup-thread-preparation-v1",
                &effect.command,
                &mut canonical_receipts,
            );
        let canonical_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-thread-preparation-effect-v2",
            artifact_fingerprint.as_str(),
            effect.lease.owner.as_str(),
            effect.lease.runtime_incarnation.as_str(),
            effect.lease.epoch,
            effect.lease.expires_at_unix_ms,
            &self.provider_prepared_effect_fence,
        ));
        let legacy_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-thread-preparation-effect-v1",
            artifact_fingerprint.as_str(),
            effect.lease.owner.as_str(),
            effect.lease.runtime_incarnation.as_str(),
            effect.lease.epoch,
            effect.lease.expires_at_unix_ms,
        ));
        let fingerprint_matches = self.receipt_fingerprint == canonical_fingerprint
            || (self.provider_prepared_effect_fence == asserted_effect_fence
                && self.receipt_fingerprint == legacy_fingerprint);
        if !receipts_are_unique
            || &self.effect != effect
            || self.artifact_receipts != canonical_receipts
            || !fingerprint_matches
        {
            return Err(SessionCleanupError::PreparationReceiptMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionCleanupProgress {
    #[serde(deserialize_with = "super::state::deserialize_persisted_operation")]
    pub(super) cleanup: SessionCleanupOperation,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) preparations: BTreeMap<String, SessionCleanupPreparation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) repository_preparation: Option<SessionCleanupRepositoryPreparation>,
}

/// Partial, per-thread durable preparation progress. Its private payload and
/// decoder forbid completed, fenced, empty, complete, or recursively wrapped
/// states, so it cannot form a second cleanup phase machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub(super) struct SessionCleanupPreparing {
    pub(super) progress: SessionCleanupProgress,
}

/// Complete preparation evidence authorizing the one root-owned physical
/// disposal. The full canonical map is retained so a lost final-preparation
/// CAS response can be verified exactly rather than accepted by phase alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub(super) struct SessionCleanupDisposing {
    pub(super) progress: SessionCleanupProgress,
}

/// Stable root-only physical disposal command derived from the aggregate's
/// exact durable preparation set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCleanupDisposalCommand {
    pub session_id: String,
    pub effect_id: String,
    /// Fingerprint of the complete terminal Runtime/Artifact/Repository
    /// preparation set. This remains distinct from the provider predecessor:
    /// a terminal takeover may inherit an earlier continuation gate.
    pub preparation_fingerprint: String,
    pub provider_disposal: awaken_provisioning_contract::SandboxDisposalPreparation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_target: Option<crate::SandboxRestoreRequest>,
}

impl SessionCleanupDisposalCommand {
    fn new(
        session_id: &str,
        root_effect_id: &str,
        preparation_fingerprint: String,
        provider_disposal: awaken_provisioning_contract::SandboxDisposalPreparation,
    ) -> Result<Self, SessionCleanupError> {
        if root_effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        let effect_id = provider_disposal
            .operation_id()
            .map_err(|_| SessionCleanupError::PreparationReceiptMismatch)?;
        Ok(Self {
            session_id: session_id.to_string(),
            effect_id,
            preparation_fingerprint,
            provider_disposal,
            restore_target: None,
        })
    }

    pub fn with_restore_target(
        mut self,
        request: crate::SandboxRestoreRequest,
    ) -> Result<Self, SessionCleanupError> {
        if request.session_id != self.session_id {
            return Err(SessionCleanupError::DisposalReceiptMismatch);
        }
        self.restore_target = Some(request);
        Ok(self)
    }
}

/// Canonical evidence that the exact prepared physical realization has been
/// disposed. Recording it transitions the same aggregate directly to its
/// existing Completed phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCleanupDisposalReceipt {
    pub session_id: String,
    pub effect_id: String,
    pub preparation_fingerprint: String,
    pub provider_disposal: awaken_provisioning_contract::SandboxDisposalPreparation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_target: Option<crate::SandboxRestoreRequest>,
    pub receipt_fingerprint: String,
}

/// The one closed Worker action projected from the durable cleanup operation.
///
/// `Waiting` retains an installed terminal assignment while the root-owned
/// Repository publication or quiescence boundary is pending. `Prepare` carries
/// only source-dependent work; `Dispose` carries the single aggregate-wide
/// physical effect. The variants are mutually exclusive so a transport cannot
/// ask one Worker to prepare and delete from the same root snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
// This is a short-lived closed wire projection, not a retained collection.
// Heap-indirecting only `Dispose` would change the public Rust action contract
// while leaving its serialized authority identical.
#[allow(clippy::large_enum_variant)]
pub enum SessionTerminalCleanupAction {
    Waiting,
    Prepare {
        commands: Vec<SessionCleanupCommand>,
    },
    Dispose {
        command: SessionCleanupDisposalCommand,
    },
}

impl SessionCleanupDisposalReceipt {
    #[must_use]
    pub fn new(command: &SessionCleanupDisposalCommand) -> Self {
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-disposal-receipt-v1",
            command.session_id.as_str(),
            command.effect_id.as_str(),
            command.preparation_fingerprint.as_str(),
            &command.provider_disposal,
            &command.restore_target,
        ));
        Self {
            session_id: command.session_id.clone(),
            effect_id: command.effect_id.clone(),
            preparation_fingerprint: command.preparation_fingerprint.clone(),
            provider_disposal: command.provider_disposal.clone(),
            restore_target: command.restore_target.clone(),
            receipt_fingerprint,
        }
    }

    fn verify(&self, command: &SessionCleanupDisposalCommand) -> Result<(), SessionCleanupError> {
        if self
            != &Self::new(
                &SessionCleanupDisposalCommand::new(
                    &command.session_id,
                    &cleanup_effect_id(&command.session_id),
                    command.preparation_fingerprint.clone(),
                    command.provider_disposal.clone(),
                )?
                .with_optional_restore_target(command.restore_target.clone())?,
            )
        {
            return Err(SessionCleanupError::DisposalReceiptMismatch);
        }
        Ok(())
    }
}

impl SessionCleanupDisposalCommand {
    fn with_optional_restore_target(
        self,
        request: Option<crate::SandboxRestoreRequest>,
    ) -> Result<Self, SessionCleanupError> {
        match request {
            Some(request) => self.with_restore_target(request),
            None => Ok(self),
        }
    }
}

struct RequestedView<'a> {
    effect_id: &'a str,
    thread_ids: &'a BTreeSet<String>,
    delegation_watermark: u64,
    runtime_commit_cursor: Option<u64>,
    completions: &'a BTreeMap<String, SessionCleanupCompletion>,
    publication_outcome_present: bool,
    publication_required: bool,
}

fn requested_view(
    cleanup: &SessionCleanupOperation,
) -> Result<RequestedView<'_>, SessionCleanupError> {
    let (cleanup, publication_outcome_present, publication_required) = match cleanup.state() {
        SessionCleanupState::RepositoryPublication(publication) => (
            &publication.cleanup,
            publication.receipt.is_some() || publication.rejection.is_some(),
            true,
        ),
        SessionCleanupState::Requested { .. } => (cleanup, false, false),
        SessionCleanupState::NotRequested
        | SessionCleanupState::Fenced { .. }
        | SessionCleanupState::Completed { .. }
        | SessionCleanupState::Preparing(_)
        | SessionCleanupState::Disposing(_) => {
            return Err(SessionCleanupError::NotRequested);
        }
    };
    let SessionCleanupState::Requested {
        effect_id,
        thread_ids,
        delegation_watermark,
        runtime_commit_cursor,
        completions,
    } = cleanup.state()
    else {
        return Err(SessionCleanupError::NotRequested);
    };
    Ok(RequestedView {
        effect_id,
        thread_ids,
        delegation_watermark: *delegation_watermark,
        runtime_commit_cursor: *runtime_commit_cursor,
        completions,
        publication_outcome_present,
        publication_required,
    })
}

impl SessionCleanupProgress {
    fn session_id(&self) -> Result<&str, SessionCleanupError> {
        self.preparations
            .values()
            .next()
            .map(|preparation| preparation.effect.command.session_id.as_str())
            .ok_or(SessionCleanupError::PreparationReceiptMismatch)
    }

    fn validate_preparation_evidence(
        &self,
        complete: bool,
        repository_preparation: Option<&SessionCleanupRepositoryPreparation>,
    ) -> Result<(), SessionCleanupError> {
        let view = requested_view(&self.cleanup)?;
        let session_id = self.session_id()?;
        if view.effect_id != cleanup_effect_id(session_id) || !view.thread_ids.contains(session_id)
        {
            return Err(SessionCleanupError::OperationMismatch);
        }
        for (thread_id, preparation) in &self.preparations {
            if thread_id != &preparation.effect.command.thread_id
                || view.completions.contains_key(thread_id)
                || !view.thread_ids.contains(thread_id)
            {
                return Err(SessionCleanupError::PreparationReceiptMismatch);
            }
            let command = SessionCleanupCommand::new(session_id, thread_id, view.effect_id);
            if preparation.effect.command.without_restore_target() != command
                || preparation
                    .effect
                    .command
                    .restore_target
                    .as_ref()
                    .is_some_and(|request| {
                        thread_id != session_id || request.session_id != session_id
                    })
            {
                return Err(SessionCleanupError::PreparationReceiptMismatch);
            }
            let effect = SessionTerminalCleanupEffect::new(
                preparation.effect.command.clone(),
                preparation.effect.lease.clone(),
            );
            preparation.verify(&effect)?;
        }
        let children_ready = view.thread_ids.iter().all(|thread_id| {
            thread_id == session_id
                || view.completions.contains_key(thread_id)
                || self.preparations.contains_key(thread_id)
        });
        if view.publication_outcome_present && !children_ready {
            return Err(SessionCleanupError::PreparationReceiptMismatch);
        }
        if self.preparations.contains_key(session_id)
            && view.publication_required
            && !view.publication_outcome_present
        {
            return Err(SessionCleanupError::PreparationReceiptMismatch);
        }
        let all_ready = view.thread_ids.iter().all(|thread_id| {
            view.completions.contains_key(thread_id) || self.preparations.contains_key(thread_id)
        }) && (!view.publication_required || view.publication_outcome_present);
        if all_ready != complete {
            return Err(if complete {
                SessionCleanupError::DisposalNotReady
            } else {
                SessionCleanupError::PreparationReceiptMismatch
            });
        }
        if complete && !self.preparations.contains_key(session_id) {
            // Historical root completion already means physical deletion and
            // uses the legacy completion path. A new Disposing state must
            // always preserve the exact root preparation fence that precedes
            // its still-pending physical effect.
            return Err(SessionCleanupError::DisposalNotReady);
        }
        match (complete, repository_preparation) {
            (true, Some(preparation)) => {
                preparation.verify_identity(session_id, view.effect_id)?;
            }
            (true, None) => return Err(SessionCleanupError::DisposalNotReady),
            (false, None) => {}
            (false, Some(_)) => {
                return Err(SessionCleanupError::RepositoryPreparationReceiptMismatch);
            }
        }
        Ok(())
    }

    fn validate(&self, complete: bool) -> Result<(), SessionCleanupError> {
        let repository_preparation = self.repository_preparation.as_ref();
        self.validate_preparation_evidence(complete, repository_preparation)?;
        match (complete, &self.repository_preparation) {
            (true, Some(_)) => {}
            (true, None) => return Err(SessionCleanupError::DisposalNotReady),
            (false, None) => {}
            (false, Some(_)) => {
                return Err(SessionCleanupError::PreparationReceiptMismatch);
            }
        }
        Ok(())
    }

    fn preparation_fingerprint(&self, session_id: &str) -> Result<String, SessionCleanupError> {
        self.validate(true)?;
        let repository = &self
            .repository_preparation
            .as_ref()
            .expect("validated complete preparation has Repository evidence");
        self.preparation_fingerprint_with_repository(session_id, repository)
    }

    fn preparation_fingerprint_with_repository(
        &self,
        session_id: &str,
        repository: &SessionCleanupRepositoryPreparation,
    ) -> Result<String, SessionCleanupError> {
        self.validate_preparation_evidence(true, Some(repository))?;
        let view = requested_view(&self.cleanup)?;
        if self.session_id()? != session_id || view.effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        let evidence = view
            .thread_ids
            .iter()
            .map(|thread_id| {
                if let Some(completion) = view.completions.get(thread_id) {
                    (
                        "legacy_completion",
                        thread_id.as_str(),
                        completion.effect_id.as_str(),
                        completion.receipt_fingerprint.as_str(),
                    )
                } else {
                    let preparation = self
                        .preparations
                        .get(thread_id)
                        .expect("validated preparation coverage is complete");
                    (
                        "preparation",
                        thread_id.as_str(),
                        preparation.effect.command.effect_id.as_str(),
                        preparation.receipt_fingerprint.as_str(),
                    )
                }
            })
            .collect::<Vec<_>>();
        let publication_outcome =
            verified_repository_publication_outcome_for(&self.cleanup, session_id)?;
        Ok(match publication_outcome.as_ref() {
            Some(VerifiedRepositoryPublicationOutcome::Published(receipt)) => {
                crate::stable_fingerprint(&(
                    "session-terminal-cleanup-preparation-set-v1",
                    view.effect_id,
                    view.delegation_watermark,
                    view.runtime_commit_cursor,
                    &evidence,
                    Some(receipt.receipt_fingerprint.as_str()),
                    repository.receipt_fingerprint(),
                ))
            }
            Some(VerifiedRepositoryPublicationOutcome::Rejected(rejection)) => {
                crate::stable_fingerprint(&(
                    "session-terminal-cleanup-preparation-set-rejection-v1",
                    view.effect_id,
                    view.delegation_watermark,
                    view.runtime_commit_cursor,
                    &evidence,
                    rejection.rejection_fingerprint.as_str(),
                    repository.receipt_fingerprint(),
                ))
            }
            None => crate::stable_fingerprint(&(
                "session-terminal-cleanup-preparation-set-v1",
                view.effect_id,
                view.delegation_watermark,
                view.runtime_commit_cursor,
                &evidence,
                Option::<&str>::None,
                repository.receipt_fingerprint(),
            )),
        })
    }
}

impl<'de> Deserialize<'de> for SessionCleanupPreparing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let progress = SessionCleanupProgress::deserialize(deserializer)?;
        progress.validate(false).map_err(serde::de::Error::custom)?;
        Ok(Self { progress })
    }
}

impl<'de> Deserialize<'de> for SessionCleanupDisposing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let progress = SessionCleanupProgress::deserialize(deserializer)?;
        progress.validate(true).map_err(serde::de::Error::custom)?;
        Ok(Self { progress })
    }
}

fn progress_for(cleanup: &SessionCleanupOperation) -> Option<&SessionCleanupProgress> {
    match cleanup.state() {
        SessionCleanupState::Preparing(preparing) => Some(&preparing.progress),
        SessionCleanupState::Disposing(disposing) => Some(&disposing.progress),
        _ => None,
    }
}

fn completed_fingerprint(
    view: &RequestedView<'_>,
    receipt: &SessionCleanupDisposalReceipt,
    publication_outcome: Option<&VerifiedRepositoryPublicationOutcome>,
) -> String {
    completed_fingerprint_from_parts(
        view.effect_id,
        view.delegation_watermark,
        view.runtime_commit_cursor,
        receipt,
        publication_outcome,
    )
}

fn completed_fingerprint_from_parts(
    effect_id: &str,
    delegation_watermark: u64,
    runtime_commit_cursor: Option<u64>,
    receipt: &SessionCleanupDisposalReceipt,
    publication_outcome: Option<&VerifiedRepositoryPublicationOutcome>,
) -> String {
    match publication_outcome {
        Some(VerifiedRepositoryPublicationOutcome::Published(publication)) => {
            crate::stable_fingerprint(&(
                "session-terminal-cleanup-prepared-receipt-v1",
                effect_id,
                delegation_watermark,
                runtime_commit_cursor,
                receipt.preparation_fingerprint.as_str(),
                receipt.receipt_fingerprint.as_str(),
                Some(publication.receipt_fingerprint.as_str()),
            ))
        }
        Some(VerifiedRepositoryPublicationOutcome::Rejected(rejection)) => {
            crate::stable_fingerprint(&(
                "session-terminal-cleanup-prepared-rejection-v1",
                effect_id,
                delegation_watermark,
                runtime_commit_cursor,
                receipt.preparation_fingerprint.as_str(),
                receipt.receipt_fingerprint.as_str(),
                rejection.rejection_fingerprint.as_str(),
            ))
        }
        None => crate::stable_fingerprint(&(
            "session-terminal-cleanup-prepared-receipt-v1",
            effect_id,
            delegation_watermark,
            runtime_commit_cursor,
            receipt.preparation_fingerprint.as_str(),
            receipt.receipt_fingerprint.as_str(),
            Option::<&str>::None,
        )),
    }
}

fn complete_progress(
    progress: &SessionCleanupProgress,
    session_id: &str,
    receipt: &SessionCleanupDisposalReceipt,
) -> Result<SessionCleanupOperation, SessionCleanupError> {
    let view = requested_view(&progress.cleanup)?;
    let publication_outcome =
        verified_repository_publication_outcome_for(&progress.cleanup, session_id)?;
    let completed = SessionCleanupOperation::from_state(SessionCleanupState::Completed {
        effect_id: view.effect_id.to_string(),
        thread_ids: view.thread_ids.clone(),
        delegation_watermark: view.delegation_watermark,
        runtime_commit_cursor: view.runtime_commit_cursor,
        receipt_fingerprint: completed_fingerprint(&view, receipt, publication_outcome.as_ref()),
    });
    match progress.cleanup.state() {
        SessionCleanupState::RepositoryPublication(publication) => {
            let mut publication = publication.as_ref().clone();
            publication.cleanup = completed;
            Ok(SessionCleanupOperation::from_state(
                SessionCleanupState::RepositoryPublication(Box::new(publication)),
            ))
        }
        SessionCleanupState::Requested { .. } => Ok(completed),
        SessionCleanupState::NotRequested
        | SessionCleanupState::Fenced { .. }
        | SessionCleanupState::Completed { .. }
        | SessionCleanupState::Preparing(_)
        | SessionCleanupState::Disposing(_) => Err(SessionCleanupError::NotRequested),
    }
}

impl SessionCleanupOperation {
    /// Project the only executable Worker action from this aggregate state.
    /// Repository publication remains a separate root-owned effect on the same
    /// Control channel; while it is pending this returns `Waiting` rather than
    /// inventing a second queue or exposing the root preparation early.
    pub(crate) fn terminal_work_action(
        &self,
        session_id: &str,
        provider_disposal: Option<&awaken_provisioning_contract::SandboxDisposalPreparation>,
    ) -> Result<Option<SessionTerminalCleanupAction>, SessionCleanupError> {
        self.verify_for(session_id)?;
        if self.is_completed() || self.is_not_requested() {
            return Ok(None);
        }
        if self.is_fenced() {
            return Ok(Some(SessionTerminalCleanupAction::Waiting));
        }
        let commands = self.pending_preparation_commands(session_id)?;
        if !commands.is_empty() {
            return Ok(Some(SessionTerminalCleanupAction::Prepare { commands }));
        }
        if let Some(command) = self.disposal_command(session_id, provider_disposal)? {
            return Ok(Some(SessionTerminalCleanupAction::Dispose { command }));
        }
        if self.publication_command(session_id)?.is_some() {
            return Ok(Some(SessionTerminalCleanupAction::Waiting));
        }
        // A legacy Requested row whose complete historical receipts already
        // prove physical disposal is normalized by the aggregate reconciler;
        // it must not be reassigned as new-style Worker preparation.
        Ok(None)
    }

    /// New-style preparation work in child-first order. Historical completion
    /// evidence is stronger and therefore covers its exact target without
    /// being reinterpreted or rewritten.
    pub(crate) fn pending_preparation_commands(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionCleanupCommand>, SessionCleanupError> {
        self.verify_for(session_id)?;
        if matches!(self.state(), SessionCleanupState::Disposing(_)) || self.is_completed() {
            return Ok(Vec::new());
        }
        let (cleanup, preparations) = match self.state() {
            SessionCleanupState::Preparing(preparing) => (
                &preparing.progress.cleanup,
                Some(&preparing.progress.preparations),
            ),
            SessionCleanupState::Requested { .. }
            | SessionCleanupState::RepositoryPublication(_) => (self, None),
            SessionCleanupState::NotRequested | SessionCleanupState::Fenced { .. } => {
                return Err(SessionCleanupError::NotRequested);
            }
            SessionCleanupState::Completed { .. } | SessionCleanupState::Disposing(_) => {
                unreachable!()
            }
        };
        let view = requested_view(cleanup)?;
        if view.effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        let is_covered = |thread_id: &String| {
            view.completions.contains_key(thread_id)
                || preparations.is_some_and(|items| items.contains_key(thread_id))
        };
        let children = view
            .thread_ids
            .iter()
            .filter(|thread_id| thread_id.as_str() != session_id && !is_covered(thread_id))
            .map(|thread_id| SessionCleanupCommand::new(session_id, thread_id, view.effect_id))
            .collect::<Vec<_>>();
        if !children.is_empty() {
            return Ok(children);
        }
        if view.publication_required && !view.publication_outcome_present {
            return Ok(Vec::new());
        }
        Ok(view
            .thread_ids
            .contains(session_id)
            .then(|| {
                (!is_covered(&session_id.to_string()))
                    .then(|| SessionCleanupCommand::new(session_id, session_id, view.effect_id))
            })
            .flatten()
            .into_iter()
            .collect())
    }

    /// Admit one exact preparation in the same aggregate. The first receipt
    /// enters Preparing; the last required receipt atomically enters
    /// Disposing, making that root CAS the physical-deletion gate.
    pub(crate) fn record_preparation(
        &mut self,
        session_id: &str,
        preparation: SessionCleanupPreparation,
        repository_preparation: Option<SessionCleanupRepositoryPreparation>,
    ) -> Result<bool, SessionCleanupError> {
        self.verify_for(session_id)?;
        let command = self
            .command_for(session_id, &preparation.effect.command.thread_id)
            .ok_or(SessionCleanupError::PreparationReceiptMismatch)?;
        if preparation.effect.command.without_restore_target() != command
            || preparation
                .effect
                .command
                .restore_target
                .as_ref()
                .is_some_and(|request| {
                    preparation.effect.command.thread_id != session_id
                        || request.session_id != session_id
                })
        {
            return Err(SessionCleanupError::PreparationReceiptMismatch);
        }
        let expected_effect = SessionTerminalCleanupEffect::new(
            preparation.effect.command.clone(),
            preparation.effect.lease.clone(),
        );
        preparation.verify(&expected_effect)?;
        if self.is_completed() {
            // Disposing retains the full map and absorbs a lost preparation
            // CAS response exactly. Completed intentionally collapses that map;
            // accepting an arbitrary late preparation here would therefore
            // admit evidence the aggregate can no longer compare.
            return Err(SessionCleanupError::PreparationReceiptMismatch);
        }
        if let SessionCleanupState::Disposing(disposing) = self.state() {
            return match (
                disposing
                    .progress
                    .preparations
                    .get(&preparation.effect.command.thread_id),
                disposing.progress.repository_preparation.as_ref(),
                repository_preparation.as_ref(),
            ) {
                (Some(durable), Some(repository), Some(asserted))
                    if durable == &preparation && repository == asserted =>
                {
                    Ok(false)
                }
                _ => Err(SessionCleanupError::PreparationReceiptMismatch),
            };
        }
        if let SessionCleanupState::Preparing(preparing) = self.state()
            && let Some(durable) = preparing
                .progress
                .preparations
                .get(&preparation.effect.command.thread_id)
        {
            return if durable == &preparation && repository_preparation.is_none() {
                Ok(false)
            } else {
                Err(SessionCleanupError::PreparationReceiptMismatch)
            };
        }
        if !self
            .pending_preparation_commands(session_id)?
            .contains(&command)
        {
            return Err(SessionCleanupError::CommandNotPending);
        }

        let (cleanup, mut preparations) = match self.state() {
            SessionCleanupState::Preparing(preparing) => (
                preparing.progress.cleanup.clone(),
                preparing.progress.preparations.clone(),
            ),
            SessionCleanupState::Requested { .. }
            | SessionCleanupState::RepositoryPublication(_) => (self.clone(), BTreeMap::new()),
            SessionCleanupState::NotRequested | SessionCleanupState::Fenced { .. } => {
                return Err(SessionCleanupError::NotRequested);
            }
            SessionCleanupState::Completed { .. } | SessionCleanupState::Disposing(_) => {
                unreachable!()
            }
        };
        let view = requested_view(&cleanup)?;
        if view
            .completions
            .contains_key(&preparation.effect.command.thread_id)
        {
            return Err(SessionCleanupError::PreparationReceiptMismatch);
        }
        match preparations.get(&preparation.effect.command.thread_id) {
            Some(durable) if durable == &preparation => return Ok(false),
            Some(_) => return Err(SessionCleanupError::PreparationReceiptMismatch),
            None => {
                preparations.insert(preparation.effect.command.thread_id.clone(), preparation);
            }
        }
        let all_ready = {
            let view = requested_view(&cleanup)?;
            view.thread_ids.iter().all(|thread_id| {
                view.completions.contains_key(thread_id) || preparations.contains_key(thread_id)
            }) && (!view.publication_required || view.publication_outcome_present)
        };
        let progress = SessionCleanupProgress {
            cleanup,
            preparations,
            repository_preparation: None,
        };
        if all_ready {
            let repository = repository_preparation
                .ok_or(SessionCleanupError::RepositoryPreparationReceiptMismatch)?;
            let mut progress = progress;
            progress.repository_preparation = Some(repository);
            progress.validate(true)?;
            *self = Self::from_state(SessionCleanupState::Disposing(Box::new(
                SessionCleanupDisposing { progress },
            )));
        } else {
            if repository_preparation.is_some() {
                return Err(SessionCleanupError::RepositoryPreparationReceiptMismatch);
            }
            progress.validate(false)?;
            *self = Self::from_state(SessionCleanupState::Preparing(Box::new(
                SessionCleanupPreparing { progress },
            )));
        }
        Ok(true)
    }

    /// Project the single physical disposal only after the aggregate is in
    /// its closed Disposing subphase.
    pub(crate) fn disposal_command(
        &self,
        session_id: &str,
        provider_disposal: Option<&awaken_provisioning_contract::SandboxDisposalPreparation>,
    ) -> Result<Option<SessionCleanupDisposalCommand>, SessionCleanupError> {
        self.verify_for(session_id)?;
        match self.state() {
            SessionCleanupState::Disposing(disposing) => {
                let view = requested_view(&disposing.progress.cleanup)?;
                let fingerprint = disposing.progress.preparation_fingerprint(session_id)?;
                let provider_disposal = provider_disposal
                    .ok_or(SessionCleanupError::DisposalNotReady)?
                    .clone();
                let command = SessionCleanupDisposalCommand::new(
                    session_id,
                    view.effect_id,
                    fingerprint,
                    provider_disposal,
                )?;
                let restore_target = disposing
                    .progress
                    .preparations
                    .get(session_id)
                    .and_then(|preparation| preparation.effect.command.restore_target.clone());
                Ok(Some(command.with_optional_restore_target(restore_target)?))
            }
            SessionCleanupState::Requested { .. }
            | SessionCleanupState::RepositoryPublication(_)
            | SessionCleanupState::Preparing(_)
            | SessionCleanupState::Completed { .. }
                if provider_disposal.is_none() =>
            {
                Ok(None)
            }
            SessionCleanupState::Requested { .. }
            | SessionCleanupState::RepositoryPublication(_)
            | SessionCleanupState::Preparing(_)
            | SessionCleanupState::Completed { .. } => Err(SessionCleanupError::DisposalNotReady),
            SessionCleanupState::NotRequested | SessionCleanupState::Fenced { .. } => {
                Err(SessionCleanupError::NotRequested)
            }
        }
    }

    /// Admit exact physical-disposal evidence and atomically return to the
    /// existing Completed phase. Completed absorbs only a receipt producing the
    /// same aggregate fingerprint, which closes final-CAS response loss.
    pub(crate) fn record_disposal(
        &mut self,
        session_id: &str,
        provider_disposal: Option<&awaken_provisioning_contract::SandboxDisposalPreparation>,
        receipt: SessionCleanupDisposalReceipt,
    ) -> Result<bool, SessionCleanupError> {
        self.verify_for(session_id)?;
        if self.is_completed() {
            let cleanup = self.legacy_cleanup();
            let SessionCleanupState::Completed {
                effect_id,
                delegation_watermark,
                runtime_commit_cursor,
                receipt_fingerprint,
                ..
            } = cleanup.state()
            else {
                unreachable!("is_completed is projected from Completed");
            };
            let command = SessionCleanupDisposalCommand::new(
                session_id,
                effect_id,
                receipt.preparation_fingerprint.clone(),
                receipt.provider_disposal.clone(),
            )?
            .with_optional_restore_target(receipt.restore_target.clone())?;
            receipt.verify(&command)?;
            let publication_outcome =
                verified_repository_publication_outcome_for(self, session_id)?;
            let expected = completed_fingerprint_from_parts(
                effect_id,
                *delegation_watermark,
                *runtime_commit_cursor,
                &receipt,
                publication_outcome.as_ref(),
            );
            return if &expected == receipt_fingerprint {
                Ok(false)
            } else {
                Err(SessionCleanupError::DisposalReceiptMismatch)
            };
        }
        let SessionCleanupState::Disposing(disposing) = self.state() else {
            return Err(SessionCleanupError::DisposalNotReady);
        };
        let progress = disposing.progress.clone();
        let command = self
            .disposal_command(session_id, provider_disposal)?
            .ok_or(SessionCleanupError::DisposalNotReady)?;
        receipt.verify(&command)?;
        let next = complete_progress(&progress, session_id, &receipt)?;
        *self = next;
        Ok(true)
    }

    pub(super) fn preparation_map(&self) -> Option<&BTreeMap<String, SessionCleanupPreparation>> {
        progress_for(self).map(|progress| &progress.preparations)
    }

    /// Durable Repository participant proof carried by the Disposing phase.
    /// Preparing and legacy operations return `None` by construction.
    #[must_use]
    pub fn repository_preparation(&self) -> Option<&SessionCleanupRepositoryPreparation> {
        progress_for(self).and_then(|progress| progress.repository_preparation.as_ref())
    }

    /// Derive the ordinary terminal provider predecessor from the root Runtime
    /// preparation and complete aggregate fingerprint. `PersistedSession` is
    /// the only owner allowed to replace this with a continuation predecessor
    /// from its frozen Environment state.
    pub(crate) fn terminal_provider_disposal_preparation(
        &self,
        session_id: &str,
    ) -> Result<Option<awaken_provisioning_contract::SandboxDisposalPreparation>, SessionCleanupError>
    {
        self.verify_for(session_id)?;
        let SessionCleanupState::Disposing(disposing) = self.state() else {
            return Ok(None);
        };
        let progress = &disposing.progress;
        let preparation_fingerprint = progress.preparation_fingerprint(session_id)?;
        let root = progress
            .preparations
            .get(session_id)
            .ok_or(SessionCleanupError::DisposalNotReady)?;
        awaken_provisioning_contract::SandboxDisposalPreparation::new(
            root.provider_prepared_effect_fence.clone(),
            preparation_fingerprint,
        )
        .map(Some)
        .map_err(|_| SessionCleanupError::PreparationReceiptMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_provisioning_contract::{
        RepositoryPublicationExpectation, RepositoryPublicationReceipt,
        RepositoryPublicationRejection,
    };
    use awaken_resource_contract::ResourceAccess;

    #[derive(serde::Deserialize)]
    struct PersistedCleanupFixture {
        #[serde(
            deserialize_with = "crate::terminal_cleanup::state::deserialize_persisted_operation"
        )]
        cleanup: SessionCleanupOperation,
    }

    fn decode_persisted_cleanup(
        value: serde_json::Value,
    ) -> Result<SessionCleanupOperation, serde_json::Error> {
        serde_json::from_value::<PersistedCleanupFixture>(serde_json::json!({
            "cleanup": value,
        }))
        .map(|fixture| fixture.cleanup)
    }

    fn decode_persisted_cleanup_slice(
        encoded: &[u8],
    ) -> Result<SessionCleanupOperation, serde_json::Error> {
        decode_persisted_cleanup(serde_json::from_slice(encoded)?)
    }

    fn preparation(command: &SessionCleanupCommand) -> SessionCleanupPreparation {
        let lease = crate::SessionRealizationLease {
            owner: "test-worker".into(),
            runtime_incarnation: "test-runtime".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        let effect = SessionTerminalCleanupEffect::new(command.clone(), lease);
        SessionCleanupPreparation::try_new(
            &effect,
            effect.sandbox_effect_fence().unwrap(),
            Vec::new(),
        )
        .unwrap()
    }

    fn repository_preparation(session_id: &str) -> SessionCleanupRepositoryPreparation {
        SessionCleanupRepositoryPreparation::new(
            session_id,
            "workspace",
            &crate::SessionResourceState::default(),
        )
        .expect("canonical empty Repository preparation")
    }

    fn disposal_command(
        cleanup: &SessionCleanupOperation,
        session_id: &str,
    ) -> Option<SessionCleanupDisposalCommand> {
        let provider = cleanup
            .terminal_provider_disposal_preparation(session_id)
            .expect("provider preparation projection");
        cleanup
            .disposal_command(session_id, provider.as_ref())
            .expect("disposal command projection")
    }

    fn terminal_action(
        cleanup: &SessionCleanupOperation,
        session_id: &str,
    ) -> Option<SessionTerminalCleanupAction> {
        let provider = cleanup
            .terminal_provider_disposal_preparation(session_id)
            .expect("provider preparation projection");
        cleanup
            .terminal_work_action(session_id, provider.as_ref())
            .expect("terminal action projection")
    }

    fn publication_intent() -> SessionRepositoryPublicationIntent {
        SessionRepositoryPublicationIntent {
            input: crate::ResolvedInput {
                binding_id: awaken_resource_contract::BindingId::from("source"),
                source: crate::ResolvedInputSource::Repository {
                    repository_id: awaken_resource_contract::RepositoryId::from("repo"),
                    config: awaken_resource_contract::RepositoryConfigVersion {
                        repository_id: awaken_resource_contract::RepositoryId::from("repo"),
                        version: awaken_resource_contract::ConfigVersion(1),
                        remote_url: "https://example.test/repo.git".into(),
                        credential_binding: None,
                        initial_branch: Some("main".into()),
                        initial_commit: None,
                        clone_policy: Default::default(),
                    },
                    credential: None,
                },
                mount_path: "/workspace/source".into(),
                access: ResourceAccess::ReadWrite,
                instructions: None,
            },
            expectation: RepositoryPublicationExpectation {
                branch: "awf/work".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                expected_prior_commit: Some("1111111111111111111111111111111111111111".into()),
            },
        }
    }

    fn publication_receipt(
        command: &SessionRepositoryPublicationCommand,
    ) -> SessionRepositoryPublicationReceipt {
        SessionRepositoryPublicationReceipt::new(
            command,
            RepositoryPublicationReceipt {
                repository_id: "repo".into(),
                source_remote_url: "https://example.test/repo.git".into(),
                branch: "awf/work".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
            },
        )
    }

    fn publication_rejection(
        command: &SessionRepositoryPublicationCommand,
    ) -> SessionRepositoryPublicationRejection {
        SessionRepositoryPublicationRejection::new(
            command,
            RepositoryPublicationRejection::RemoteRefAbsent,
        )
        .unwrap()
    }
}
