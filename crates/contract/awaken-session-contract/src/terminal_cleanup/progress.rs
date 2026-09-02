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

    #[test]
    fn preparation_is_durable_before_the_single_physical_disposal() {
        // Cause/effect graph: C1 each frozen thread has no preparation, an
        // exact preparation, or a historical physical completion; C2 the
        // operation-wide physical disposal is unavailable/ready; C3 a root
        // CAS response may be lost after the last preparation or disposal.
        // Effects: E1 child-first preparation remains the only initial work;
        // E2 physical disposal is absent before all preparation is durable;
        // E3 the last preparation CAS enters Disposing; E4 exact preparation
        // and disposal replays are no-ops; E5 disposal CAS alone enters
        // the existing Completed phase.
        //
        // | Rule | thread evidence | durable phase | retry | Effect |
        // | P1 | none | Requested | no | child prepare only/E1/E2 |
        // | P2 | child exact | Preparing | restart | root prepare only/E2 |
        // | P3 | all exact | Disposing | last-prep retry | E3/E4 |
        // | P4 | all exact | Disposing | foreign dispose | reject/no change |
        // | P5 | all exact | Completed | final-CAS retry | E4/E5 |
        let mut state = SessionCleanupOperation::default();
        assert!(state.request("prepared-session"));
        assert!(
            state
                .freeze_targets("prepared-session", ["prepared-child".to_string()], 19, 23,)
                .unwrap()
        );
        let child = state
            .command_for("prepared-session", "prepared-child")
            .unwrap();
        let root = state
            .command_for("prepared-session", "prepared-session")
            .unwrap();
        assert_eq!(
            state
                .pending_preparation_commands("prepared-session")
                .unwrap(),
            vec![child.clone()],
            "P1/E1"
        );
        assert!(
            disposal_command(&state, "prepared-session").is_none(),
            "P1/E2"
        );

        let child_preparation = preparation(&child);
        assert!(
            state
                .record_preparation("prepared-session", child_preparation, None)
                .unwrap(),
            "P2"
        );
        let encoded = serde_json::to_vec(&state).unwrap();
        let mut recovered = decode_persisted_cleanup_slice(&encoded).unwrap();
        assert_eq!(
            recovered
                .pending_preparation_commands("prepared-session")
                .unwrap(),
            vec![root.clone()],
            "P2/E2"
        );

        let root_preparation = preparation(&root);
        assert!(
            recovered
                .record_preparation(
                    "prepared-session",
                    root_preparation.clone(),
                    Some(repository_preparation("prepared-session")),
                )
                .unwrap(),
            "P3"
        );
        assert!(
            !recovered
                .record_preparation(
                    "prepared-session",
                    root_preparation,
                    Some(repository_preparation("prepared-session")),
                )
                .unwrap(),
            "P3/E4 last preparation response loss"
        );
        let disposal = disposal_command(&recovered, "prepared-session").expect("P3/E3");
        let encoded = serde_json::to_vec(&recovered).unwrap();
        let mut recovered = decode_persisted_cleanup_slice(&encoded).unwrap();
        assert_eq!(
            disposal_command(&recovered, "prepared-session"),
            Some(disposal.clone()),
            "P3/E3"
        );

        let mut conflicting = SessionCleanupDisposalReceipt::new(&disposal);
        conflicting.preparation_fingerprint.push_str("-foreign");
        assert_eq!(
            recovered.record_disposal(
                "prepared-session",
                Some(&disposal.provider_disposal),
                conflicting,
            ),
            Err(SessionCleanupError::DisposalReceiptMismatch),
            "P4"
        );
        assert!(recovered.is_requested(), "P4");

        let exact = SessionCleanupDisposalReceipt::new(&disposal);
        assert!(
            recovered
                .record_disposal(
                    "prepared-session",
                    Some(&disposal.provider_disposal),
                    exact.clone(),
                )
                .unwrap(),
            "P5/E5"
        );
        assert!(recovered.is_completed(), "P5/E5");
        assert!(
            !recovered
                .record_disposal("prepared-session", None, exact)
                .unwrap(),
            "P5/E4 final CAS response loss"
        );
    }

    #[test]
    fn terminal_worker_action_is_closed_over_one_durable_phase() {
        // Cause/effect graph: C1 the aggregate is fenced, preparing children,
        // waiting for Repository publication, fully prepared, historically
        // complete, or durably Completed; C2 only one root snapshot may project
        // Worker work. Effects: E1 retain the assignment without I/O; E2 expose
        // only pending preparation commands; E3 expose one physical disposer;
        // E4 expose no v2 Worker work for legacy/completed rows. Decision rules:
        //
        // | Rule | durable state | Effect |
        // | A1 | Fenced | Waiting / E1 |
        // | A2 | Requested, child missing | Prepare(child) / E2 |
        // | A3 | child prepared, publication missing | Waiting / E1 |
        // | A4 | all prepared | Dispose(root aggregate) / E3 |
        // | A5 | historical or new Completed | None / E4 |
        let mut state = SessionCleanupOperation::default();
        assert!(state.request("action-session"));
        assert_eq!(
            terminal_action(&state, "action-session"),
            Some(SessionTerminalCleanupAction::Waiting),
            "A1/E1"
        );
        state
            .freeze_targets("action-session", ["action-child".into()], 5, 8)
            .unwrap();
        let child = state.command_for("action-session", "action-child").unwrap();
        assert_eq!(
            terminal_action(&state, "action-session"),
            Some(SessionTerminalCleanupAction::Prepare {
                commands: vec![child.clone()]
            }),
            "A2/E2"
        );

        let root = state
            .command_for("action-session", "action-session")
            .unwrap();
        state
            .record_preparation("action-session", preparation(&child), None)
            .unwrap();
        assert_eq!(
            terminal_action(&state, "action-session"),
            Some(SessionTerminalCleanupAction::Prepare {
                commands: vec![root.clone()]
            }),
            "A2/E2 root"
        );
        state
            .record_preparation(
                "action-session",
                preparation(&root),
                Some(repository_preparation("action-session")),
            )
            .unwrap();
        let disposal = disposal_command(&state, "action-session").expect("A4/E3");
        assert_eq!(
            terminal_action(&state, "action-session"),
            Some(SessionTerminalCleanupAction::Dispose {
                command: disposal.clone()
            }),
            "A4/E3"
        );
        state
            .record_disposal(
                "action-session",
                Some(&disposal.provider_disposal),
                SessionCleanupDisposalReceipt::new(&disposal),
            )
            .unwrap();
        assert_eq!(terminal_action(&state, "action-session"), None, "A5/E4");

        let mut publication = SessionCleanupOperation::default();
        publication
            .request_with_publication("publication-session", publication_intent())
            .unwrap();
        publication
            .freeze_targets("publication-session", ["publication-child".into()], 5, 8)
            .unwrap();
        let child = publication
            .command_for("publication-session", "publication-child")
            .unwrap();
        publication
            .record_preparation("publication-session", preparation(&child), None)
            .unwrap();
        assert_eq!(
            terminal_action(&publication, "publication-session"),
            Some(SessionTerminalCleanupAction::Waiting),
            "A3/E1"
        );
    }

    #[test]
    fn preparation_progress_has_one_closed_wire_and_one_repository_barrier() {
        // Cause/effect graph: C1 progress is partial/complete; C2 Repository
        // publication is absent/durable receipt/durable CAS rejection; C3 the wire claims Preparing or
        // Disposing; C4 the wrapped operation is exact/nested. Effects: E1 a
        // partial map decodes only as Preparing; E2 a complete map decodes only
        // as Disposing; E3 child preparation exposes the existing publication
        // command, while root preparation remains withheld until either verified outcome;
        // E4 invalid phase/map combinations fail decode without a fallback; E5
        // only the private aggregate-field codec can decode these wire states;
        // E6 either verified outcome is bound into disposal completion and exact
        // final-CAS replay without invoking publication again.
        //
        // | Rule | coverage | publication | wire state | Effect |
        // | W1 | child partial | absent | Preparing | E1/E3 |
        // | W2 | child partial | receipt | Preparing | root prepare next/E3 |
        // | W2R | child partial | rejection | Preparing -> Completed | E3/E6 |
        // | W3 | complete | exact | Disposing | E2 |
        // | W4 | partial | any | Disposing | E4 |
        // | W5 | complete | any | Preparing | E4 |
        // | W6 | complete | exact | Disposing + unknown sibling | E4/E5 |
        let mut state = SessionCleanupOperation::default();
        state
            .request_with_publication("repo-session", publication_intent())
            .unwrap();
        state
            .freeze_targets("repo-session", ["repo-child".into()], 4, 8)
            .unwrap();
        let child = state.command_for("repo-session", "repo-child").unwrap();
        state
            .record_preparation("repo-session", preparation(&child), None)
            .unwrap();
        let partial = serde_json::to_value(&state).unwrap();
        assert_eq!(partial["state"], "preparing", "W1/E1");
        assert!(decode_persisted_cleanup(partial.clone()).is_ok(), "W1/E1");
        let publication = state
            .publication_command("repo-session")
            .unwrap()
            .expect("W1/E3");
        let mut rejected = state.clone();
        rejected
            .record_repository_publication_rejection(
                "repo-session",
                publication_rejection(&publication),
            )
            .unwrap();
        let rejected_root = rejected
            .command_for("repo-session", "repo-session")
            .unwrap();
        rejected
            .record_preparation(
                "repo-session",
                preparation(&rejected_root),
                Some(repository_preparation("repo-session")),
            )
            .unwrap();
        let rejected_disposal = disposal_command(&rejected, "repo-session").expect("W2R disposal");
        let rejected_disposal_receipt = SessionCleanupDisposalReceipt::new(&rejected_disposal);
        assert!(
            rejected
                .record_disposal(
                    "repo-session",
                    Some(&rejected_disposal.provider_disposal),
                    rejected_disposal_receipt.clone(),
                )
                .unwrap(),
            "W2R/E6 rejection reaches the one Completed phase"
        );
        assert!(
            !rejected
                .record_disposal(
                    "repo-session",
                    Some(&rejected_disposal.provider_disposal),
                    rejected_disposal_receipt,
                )
                .unwrap(),
            "W2R/E6 exact final-CAS replay is absorbed"
        );
        assert!(
            rejected
                .repository_publication_rejection("repo-session")
                .unwrap()
                .is_some(),
            "W2R/E6 rejection remains the verified sidecar outcome"
        );
        let root = state.command_for("repo-session", "repo-session").unwrap();
        assert_eq!(
            state.record_preparation("repo-session", preparation(&root), None),
            Err(SessionCleanupError::CommandNotPending),
            "W1/E3"
        );
        state
            .record_repository_publication_receipt(
                "repo-session",
                publication_receipt(&publication),
            )
            .unwrap();
        assert_eq!(
            state.pending_preparation_commands("repo-session").unwrap(),
            vec![root.clone()],
            "W2/E3"
        );
        state
            .record_preparation(
                "repo-session",
                preparation(&root),
                Some(repository_preparation("repo-session")),
            )
            .unwrap();
        let complete = serde_json::to_value(&state).unwrap();
        assert_eq!(complete["state"], "disposing", "W3/E2");
        assert!(decode_persisted_cleanup(complete.clone()).is_ok(), "W3/E2");

        let mut partial_as_disposing = partial;
        partial_as_disposing["state"] = serde_json::json!("disposing");
        assert!(
            decode_persisted_cleanup(partial_as_disposing).is_err(),
            "W4/E4"
        );
        let mut complete_as_preparing = complete.clone();
        complete_as_preparing["state"] = serde_json::json!("preparing");
        assert!(
            decode_persisted_cleanup(complete_as_preparing).is_err(),
            "W5/E4"
        );
        let mut unknown_sibling = complete;
        unknown_sibling["progress"]["parallel_provider_preparation"] =
            serde_json::json!({"forged": true});
        assert!(
            decode_persisted_cleanup(unknown_sibling).is_err(),
            "W6/E4 closed progress rejects a parallel authority field"
        );
    }

    #[test]
    fn physical_disposal_carries_the_exact_durable_preparation_effect() {
        // Cause/effect graph: C1 the root preparation ran under lease A; C2 the
        // current disposer has lease A, a monotonic renewal, or reassigned
        // lease B; C3 the disposal command is exact or was forged without
        // the durable predecessor. Effects: E1 preparation persists its exact
        // command+lease; E2 Disposing projects that predecessor plus the
        // complete preparation fingerprint; E3 current ownership remains a
        // separate successor fence; E4 a raw preparation operation never
        // becomes a physical-disposal command.
        //
        // | Rule | prepared evidence | current lease | Effect |
        // | F1 | exact root effect A | A | command embeds A / E1-E3 |
        // | F2 | exact root effect A | renewed/reassigned | command still embeds A / E2-E3 |
        // | F3 | command only, no stored lease | any | cannot construct / E4 |
        let prepared_lease = crate::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "runtime-a".into(),
            epoch: 4,
            expires_at_unix_ms: 40_000,
        };
        let mut state = SessionCleanupOperation::default();
        assert!(state.request("prepared-root"));
        state.freeze_targets("prepared-root", [], 7, 11).unwrap();
        let command = state.command_for("prepared-root", "prepared-root").unwrap();
        let prepared_effect = SessionTerminalCleanupEffect::new(command, prepared_lease.clone());
        state
            .record_preparation(
                "prepared-root",
                SessionCleanupPreparation::try_new(
                    &prepared_effect,
                    prepared_effect.sandbox_effect_fence().unwrap(),
                    Vec::new(),
                )
                .unwrap(),
                Some(repository_preparation("prepared-root")),
            )
            .unwrap();

        let disposal = disposal_command(&state, "prepared-root").expect("F1/E2");
        assert_eq!(
            disposal.provider_disposal.prepared_effect_fence(),
            &prepared_lease
                .sandbox_effect_fence(prepared_effect.command.effect_id.clone())
                .unwrap(),
            "F2/E2 predecessor cannot be reconstructed from a successor lease"
        );
        assert_ne!(
            disposal.effect_id, prepared_effect.command.effect_id,
            "F3/E4 preparation and physical disposal are distinct effects"
        );
    }

    #[test]
    fn preparation_wire_closes_provider_predecessor_while_decoding_exact_legacy_bytes() {
        // Wire decision table W1. Causes: C1 P is present/omitted; C2 P equals
        // the work fence or is a distinct same-generation renewal; C3 the
        // fingerprint uses legacy v1 or current v2. Effects: E1 omitted P plus
        // v1 decodes as exact P=effect for old writers; E2 current v2 binds
        // explicit P; E3 v1 with explicit distinct P is rejected. No Option is
        // retained in the domain value or emitted by a current writer.
        let mut operation = SessionCleanupOperation::default();
        assert!(operation.request("wire-session"));
        operation.freeze_targets("wire-session", [], 1, 1).unwrap();
        let command = operation
            .command_for("wire-session", "wire-session")
            .unwrap();
        let lease = crate::SessionRealizationLease {
            owner: "wire-owner".into(),
            runtime_incarnation: "wire-runtime".into(),
            epoch: 3,
            expires_at_unix_ms: 10_000,
        };
        let effect = SessionTerminalCleanupEffect::new(command, lease.clone());
        let asserted = effect.sandbox_effect_fence().unwrap();
        let mut legacy =
            SessionCleanupPreparation::try_new(&effect, asserted.clone(), Vec::new()).unwrap();
        let mut receipts = Vec::new();
        let (_, artifact_fingerprint) = canonical_thread_artifact_receipt_fingerprint(
            "session-terminal-cleanup-thread-preparation-v1",
            &effect.command,
            &mut receipts,
        );
        legacy.receipt_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-thread-preparation-effect-v1",
            artifact_fingerprint.as_str(),
            lease.owner.as_str(),
            lease.runtime_incarnation.as_str(),
            lease.epoch,
            lease.expires_at_unix_ms,
        ));
        let mut legacy_wire = serde_json::to_value(&legacy).unwrap();
        legacy_wire
            .as_object_mut()
            .unwrap()
            .remove("provider_prepared_effect_fence");
        let decoded: SessionCleanupPreparation = serde_json::from_value(legacy_wire).unwrap();
        assert_eq!(decoded.provider_prepared_effect_fence(), &asserted, "W1/E1",);

        let renewed = crate::SessionRealizationLease {
            expires_at_unix_ms: 20_000,
            ..lease
        }
        .sandbox_effect_fence(effect.operation_id())
        .unwrap();
        let mut forged = serde_json::to_value(&legacy).unwrap();
        forged["provider_prepared_effect_fence"] = serde_json::to_value(renewed).unwrap();
        assert!(
            serde_json::from_value::<SessionCleanupPreparation>(forged).is_err(),
            "W1/E3",
        );
    }
}
