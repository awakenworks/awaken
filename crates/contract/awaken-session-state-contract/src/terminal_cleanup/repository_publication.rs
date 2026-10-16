// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the canonical Session terminal-cleanup sidecar for exactly one
// @formal reason: command-bound Repository publication outcome. Dynamic path: a verified receipt or
// @formal reason: permanent rejection is recorded mutually exclusively before ordinary root cleanup, exact
// @formal reason: replay returns the same durable outcome, and unavailable effects retain the pending
// @formal reason: command without a second queue, store, or state machine. Evidence:
// @formal reason: session.lifecycle.session_state, G48, and the adjacent decoder and decision-table tests;
// @formal reason: this is a product-requirement classification, not a direct formal-proof claim.
//! Exact Repository publication effect vocabulary and sidecar codec.
//!
//! This module owns the secret-free intent, command, receipt verification, and
//! one-level cleanup wrapper. Aggregate archive admission remains solely in
//! `session_repo::repository_publication`; the generic cleanup phase machine
//! remains solely in the parent module.

use super::state::SessionCleanupState;
use super::{SessionCleanupError, SessionCleanupOperation, cleanup_effect_id};
use crate::SessionId;
use awaken_provisioning_contract::{
    RepositoryPublicationExpectation, RepositoryPublicationReceipt, RepositoryPublicationRejection,
};
use awaken_resource_contract::ResourceAccess;
use serde::{Deserialize, Serialize};

/// One explicit, frozen request to publish the exact writable Repository input
/// already owned by the Session. Configuration, credential reference, mount,
/// and access remain in the existing [`crate::ResolvedInput`] authority; this
/// intent adds only the terminal Git ref expected by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRepositoryPublicationIntent {
    pub input: crate::ResolvedInput,
    pub expectation: RepositoryPublicationExpectation,
}

/// Stable root-only publication command derived from the one durable cleanup
/// operation. It contains no credential material and cannot select a current
/// Repository configuration after the Session has frozen its input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRepositoryPublicationCommand {
    pub session_id: crate::SessionId,
    pub effect_id: String,
    pub intent: SessionRepositoryPublicationIntent,
}

/// Untrusted but canonical, secret-free effect evidence for one exact Session
/// publication command. The Repository result remains owned by the provisioning
/// contract; this wrapper binds it to the durable Session operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRepositoryPublicationReceipt {
    pub command_fingerprint: String,
    pub effect_receipt: RepositoryPublicationReceipt,
    pub receipt_fingerprint: String,
}

/// Canonical permanent rejection evidence for one exact Session publication
/// command. This is the negative counterpart of the publication receipt, not a
/// second cleanup state: exactly one of the two may be retained by the existing
/// publication sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRepositoryPublicationRejection {
    pub command_fingerprint: String,
    pub effect_rejection: SessionRepositoryPublicationRejectionReason,
    pub rejection_fingerprint: String,
}

/// Negative Session publication evidence. The original transport CAS wire is
/// unchanged; source closure is a Worker-observed prerequisite failure and says
/// nothing about whether an earlier remote publication happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(untagged, deny_unknown_fields)]
pub enum SessionRepositoryPublicationRejectionReason {
    #[error("{0}")]
    CompareAndSwap(RepositoryPublicationRejection),
    #[error("Repository publication source is closed; remote publication outcome is unknown")]
    SourceClosed {
        source_binding_fingerprint: String,
        observation: awaken_provisioning_contract::SandboxObservation,
    },
}

impl SessionRepositoryPublicationRejectionReason {
    fn verify(
        &self,
        expectation: &RepositoryPublicationExpectation,
    ) -> Result<(), SessionCleanupError> {
        let valid = match self {
            Self::CompareAndSwap(rejection) => rejection.verify(expectation).is_ok(),
            Self::SourceClosed {
                source_binding_fingerprint,
                observation,
            } => expectation.validate().is_ok()
                && !source_binding_fingerprint.trim().is_empty()
                && match observation {
                    awaken_provisioning_contract::SandboxObservation::DefinitivelyUnavailable {
                        physical_incarnation,
                    } => physical_incarnation
                        .as_ref()
                        .is_none_or(|incarnation| !incarnation.trim().is_empty()),
                    awaken_provisioning_contract::SandboxObservation::Terminal {
                        physical_incarnation,
                    } => !physical_incarnation.trim().is_empty(),
                    _ => false,
                },
        };
        if valid {
            Ok(())
        } else {
            Err(SessionCleanupError::RepositoryPublicationRejectionMismatch)
        }
    }

    fn binding_fingerprint(binding: &str) -> String {
        crate::stable_fingerprint(&("session-repository-publication-source-v1", binding))
    }

    /// Exact aggregate source comparison; provider payload interpretation and
    /// physical-incarnation validation remain exclusively Worker-owned.
    pub(crate) fn matches_source_binding(&self, binding: Option<&str>) -> bool {
        match self {
            Self::CompareAndSwap(_) => true,
            Self::SourceClosed {
                source_binding_fingerprint,
                ..
            } => binding.is_some_and(|binding| {
                *source_binding_fingerprint == Self::binding_fingerprint(binding)
            }),
        }
    }

    #[must_use]
    pub fn is_source_closed(&self) -> bool {
        matches!(self, Self::SourceClosed { .. })
    }
}

/// One execution attempt at the root publication effect. A permanent CAS veto
/// or exact source-closure observation is durable before ordinary root cleanup.
/// Source closure preserves unknown remote outcome; it is never success or CAS
/// conflict evidence. Indeterminate dependency/observation errors remain outside
/// this enum and retain the same command for retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRepositoryPublicationEffect {
    Published(SessionRepositoryPublicationReceipt),
    Rejected(SessionRepositoryPublicationRejection),
}

/// The publication sidecar around the one legacy cleanup operation.
///
/// Its fields are private so callers cannot construct a recursive wrapper or a
/// second cleanup state machine. The inner operation remains the sole phase,
/// target, completion, and terminal-receipt authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionRepositoryPublicationCleanup {
    pub(super) cleanup: SessionCleanupOperation,
    pub(super) intent: SessionRepositoryPublicationIntent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) receipt: Option<SessionRepositoryPublicationReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) rejection: Option<SessionRepositoryPublicationRejection>,
}

#[derive(Clone)]
pub(super) enum VerifiedRepositoryPublicationOutcome {
    Published(SessionRepositoryPublicationReceipt),
    Rejected(SessionRepositoryPublicationRejection),
}

pub(super) fn verified_repository_publication_outcome(
    publication: &SessionRepositoryPublicationCleanup,
    command: &SessionRepositoryPublicationCommand,
) -> Result<VerifiedRepositoryPublicationOutcome, SessionCleanupError> {
    match (&publication.receipt, &publication.rejection) {
        (Some(receipt), None) => {
            receipt.verify(command)?;
            Ok(VerifiedRepositoryPublicationOutcome::Published(
                receipt.clone(),
            ))
        }
        (None, Some(rejection)) => {
            rejection.verify(command)?;
            Ok(VerifiedRepositoryPublicationOutcome::Rejected(
                rejection.clone(),
            ))
        }
        (None, None) => Err(SessionCleanupError::MissingRepositoryPublicationOutcome),
        (Some(_), Some(_)) => Err(SessionCleanupError::RepositoryPublicationOutcomeMismatch),
    }
}

/// Resolve the optional publication sidecar only through the same outer-Session
/// verification used by persistence, tombstone admission, and reconciliation.
pub(super) fn verified_repository_publication_outcome_for(
    cleanup: &SessionCleanupOperation,
    session_id: &SessionId,
) -> Result<Option<VerifiedRepositoryPublicationOutcome>, SessionCleanupError> {
    cleanup.verify_for(session_id)?;
    let envelope = cleanup.progress_inner();
    let SessionCleanupState::RepositoryPublication(publication) = envelope.state() else {
        return Ok(None);
    };
    let effect_id = publication
        .cleanup
        .effect_id()
        .ok_or(SessionCleanupError::NotRequested)?;
    let command =
        SessionRepositoryPublicationCommand::new(session_id, effect_id, &publication.intent)?;
    verified_repository_publication_outcome(publication, &command).map(Some)
}

impl SessionCleanupOperation {
    /// Bind the retained cleanup operation and any publication sidecar to their Session.
    ///
    /// The sidecar decoder can verify intent-local fingerprints, but the
    /// Session id is deliberately owned by crate::PersistedSession. The public
    /// aggregate boundary is `PersistedSession::verified_terminal_cleanup`;
    /// crate-internal projections reuse this kernel before a Completed phase
    /// can suppress work or authorize a tombstone.
    pub(crate) fn verify_for(&self, session_id: &SessionId) -> Result<(), SessionCleanupError> {
        self.verify_identity_for(session_id)?;
        let envelope = self.progress_inner();
        let SessionCleanupState::RepositoryPublication(publication) = envelope.state() else {
            return Ok(());
        };
        publication.intent.validate()?;
        match publication.cleanup.state() {
            SessionCleanupState::Fenced { .. } => {
                if publication.receipt.is_some() || publication.rejection.is_some() {
                    return Err(SessionCleanupError::RepositoryPublicationOutcomeMismatch);
                }
            }
            SessionCleanupState::Requested { effect_id, .. } => {
                if publication.receipt.is_some() || publication.rejection.is_some() {
                    let command = SessionRepositoryPublicationCommand::new(
                        session_id,
                        effect_id,
                        &publication.intent,
                    )?;
                    verified_repository_publication_outcome(publication, &command)?;
                }
            }
            SessionCleanupState::Completed { effect_id, .. } => {
                let command = SessionRepositoryPublicationCommand::new(
                    session_id,
                    effect_id,
                    &publication.intent,
                )?;
                verified_repository_publication_outcome(publication, &command)?;
            }
            SessionCleanupState::NotRequested
            | SessionCleanupState::RepositoryPublication(_)
            | SessionCleanupState::Preparing(_)
            | SessionCleanupState::Disposing(_) => {
                return Err(SessionCleanupError::FrozenRepositoryPublicationMismatch);
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn repository_publication_intent(&self) -> Option<&SessionRepositoryPublicationIntent> {
        match self.progress_inner().state() {
            SessionCleanupState::RepositoryPublication(publication) => Some(&publication.intent),
            _ => None,
        }
    }

    /// Project the one root Repository publication effect only after every
    /// child Runtime cleanup receipt or preparation is durable and before the
    /// root cleanup may dispose the shared environment.
    pub fn publication_command(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionRepositoryPublicationCommand>, SessionCleanupError> {
        self.verify_for(session_id)?;
        let envelope = self.progress_inner();
        let SessionCleanupState::RepositoryPublication(publication) = envelope.state() else {
            return match envelope.state() {
                SessionCleanupState::Requested { .. } | SessionCleanupState::Completed { .. } => {
                    Ok(None)
                }
                SessionCleanupState::NotRequested | SessionCleanupState::Fenced { .. } => {
                    Err(SessionCleanupError::NotRequested)
                }
                SessionCleanupState::RepositoryPublication(_) => unreachable!(),
                SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_) => {
                    unreachable!("cleanup progress wrappers cannot be nested")
                }
            };
        };
        let SessionCleanupState::Requested {
            effect_id,
            thread_ids,
            completions,
            ..
        } = publication.cleanup.state()
        else {
            return if publication.cleanup.is_completed() {
                Ok(None)
            } else {
                Err(SessionCleanupError::NotRequested)
            };
        };
        if effect_id != &cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        if !thread_ids.contains(session_id.as_str()) {
            return Err(SessionCleanupError::FrozenTargetsMismatch);
        }
        if thread_ids.iter().any(|thread_id| {
            thread_id != session_id
                && !completions.contains_key(thread_id)
                && !self
                    .preparation_map()
                    .is_some_and(|items| items.contains_key(thread_id))
        }) {
            return Ok(None);
        }
        let command =
            SessionRepositoryPublicationCommand::new(session_id, effect_id, &publication.intent)?;
        if publication.receipt.is_some() || publication.rejection.is_some() {
            verified_repository_publication_outcome(publication, &command)?;
            return Ok(None);
        }
        Ok(Some(command))
    }

    pub fn repository_publication_receipt(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<&SessionRepositoryPublicationReceipt>, SessionCleanupError> {
        self.verify_for(session_id)?;
        let envelope = self.progress_inner();
        let SessionCleanupState::RepositoryPublication(publication) = envelope.state() else {
            return Ok(None);
        };
        let Some(receipt) = publication.receipt.as_ref() else {
            return Ok(None);
        };
        if publication.rejection.is_some() {
            return Err(SessionCleanupError::RepositoryPublicationOutcomeMismatch);
        }
        let effect_id = publication
            .cleanup
            .effect_id()
            .ok_or(SessionCleanupError::NotRequested)?;
        let command =
            SessionRepositoryPublicationCommand::new(session_id, effect_id, &publication.intent)?;
        receipt.verify(&command)?;
        Ok(Some(receipt))
    }

    pub fn repository_publication_rejection(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<&SessionRepositoryPublicationRejection>, SessionCleanupError> {
        self.verify_for(session_id)?;
        let envelope = self.progress_inner();
        let SessionCleanupState::RepositoryPublication(publication) = envelope.state() else {
            return Ok(None);
        };
        let Some(rejection) = publication.rejection.as_ref() else {
            return Ok(None);
        };
        if publication.receipt.is_some() {
            return Err(SessionCleanupError::RepositoryPublicationOutcomeMismatch);
        }
        let effect_id = publication
            .cleanup
            .effect_id()
            .ok_or(SessionCleanupError::NotRequested)?;
        let command =
            SessionRepositoryPublicationCommand::new(session_id, effect_id, &publication.intent)?;
        rejection.verify(&command)?;
        Ok(Some(rejection))
    }

    /// Verify and durably retain the exact root Repository publication receipt.
    /// Children must already be complete. Exact replay is a no-op, while a
    /// different command binding or effect receipt fails closed.
    pub fn record_repository_publication_receipt(
        &mut self,
        session_id: &SessionId,
        receipt: SessionRepositoryPublicationReceipt,
    ) -> Result<bool, SessionCleanupError> {
        self.record_repository_publication_effect(
            session_id,
            SessionRepositoryPublicationEffect::Published(receipt),
        )
    }

    /// Verify and retain negative publication evidence in the same
    /// sidecar as a successful publication receipt. Once durable, the ordinary
    /// root cleanup command may run; exact replay cannot invoke Git again.
    pub(crate) fn record_repository_publication_rejection(
        &mut self,
        session_id: &SessionId,
        rejection: SessionRepositoryPublicationRejection,
    ) -> Result<bool, SessionCleanupError> {
        self.record_repository_publication_effect(
            session_id,
            SessionRepositoryPublicationEffect::Rejected(rejection),
        )
    }

    fn record_repository_publication_effect(
        &mut self,
        session_id: &SessionId,
        effect: SessionRepositoryPublicationEffect,
    ) -> Result<bool, SessionCleanupError> {
        let preparation_threads = self
            .preparation_map()
            .map(|items| {
                items
                    .keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .unwrap_or_default();
        if !matches!(
            self.progress_inner().state(),
            SessionCleanupState::RepositoryPublication(_)
        ) {
            return match self.legacy_cleanup().state() {
                SessionCleanupState::Fenced { .. } => {
                    Err(SessionCleanupError::RepositoryPublicationNotReady)
                }
                SessionCleanupState::NotRequested
                | SessionCleanupState::Requested { .. }
                | SessionCleanupState::Completed { .. } => {
                    Err(SessionCleanupError::RepositoryPublicationNotRequested)
                }
                SessionCleanupState::RepositoryPublication(_)
                | SessionCleanupState::Preparing(_)
                | SessionCleanupState::Disposing(_) => {
                    unreachable!("cleanup wrappers are removed before classification")
                }
            };
        }
        let envelope = self.progress_inner_mut();
        let SessionCleanupState::RepositoryPublication(publication) = envelope.state_mut() else {
            unreachable!("publication wrapper was checked above")
        };
        let publication = publication.as_mut();
        let (effect_id, root_present, children_complete, completed) =
            match publication.cleanup.state() {
                SessionCleanupState::Requested {
                    effect_id,
                    thread_ids,
                    completions,
                    ..
                } => (
                    effect_id.clone(),
                    thread_ids.contains(session_id.as_str()),
                    thread_ids.iter().all(|thread_id| {
                        thread_id == session_id
                            || completions.contains_key(thread_id)
                            || preparation_threads.contains(thread_id)
                    }),
                    false,
                ),
                SessionCleanupState::Completed { effect_id, .. } => {
                    (effect_id.clone(), true, true, true)
                }
                SessionCleanupState::NotRequested => {
                    return Err(SessionCleanupError::RepositoryPublicationNotRequested);
                }
                SessionCleanupState::Fenced { .. } => {
                    return Err(SessionCleanupError::RepositoryPublicationNotReady);
                }
                SessionCleanupState::RepositoryPublication(_)
                | SessionCleanupState::Preparing(_)
                | SessionCleanupState::Disposing(_) => {
                    return Err(SessionCleanupError::FrozenRepositoryPublicationMismatch);
                }
            };
        if effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        if !root_present {
            return Err(SessionCleanupError::FrozenTargetsMismatch);
        }
        if !children_complete {
            return Err(SessionCleanupError::RepositoryPublicationNotReady);
        }
        let command =
            SessionRepositoryPublicationCommand::new(session_id, &effect_id, &publication.intent)?;
        match effect {
            SessionRepositoryPublicationEffect::Published(receipt) => {
                receipt.verify(&command)?;
                if publication.rejection.is_some() {
                    return Err(SessionCleanupError::RepositoryPublicationOutcomeMismatch);
                }
                match publication.receipt.as_ref() {
                    Some(durable) if durable == &receipt => return Ok(false),
                    Some(_) => {
                        return Err(SessionCleanupError::RepositoryPublicationReceiptMismatch);
                    }
                    None if completed => {
                        return Err(SessionCleanupError::MissingRepositoryPublicationOutcome);
                    }
                    None => {}
                }
                publication.receipt = Some(receipt);
            }
            SessionRepositoryPublicationEffect::Rejected(rejection) => {
                rejection.verify(&command)?;
                if publication.receipt.is_some() {
                    return Err(SessionCleanupError::RepositoryPublicationOutcomeMismatch);
                }
                match publication.rejection.as_ref() {
                    Some(durable) if durable == &rejection => return Ok(false),
                    Some(_) => {
                        return Err(SessionCleanupError::RepositoryPublicationRejectionMismatch);
                    }
                    None if completed => {
                        return Err(SessionCleanupError::MissingRepositoryPublicationOutcome);
                    }
                    None => {}
                }
                publication.rejection = Some(rejection);
            }
        }
        Ok(true)
    }
}

impl SessionRepositoryPublicationIntent {
    /// Reject any value that is not exactly one writable Repository input and
    /// one canonical full-commit publication expectation.
    pub fn validate(&self) -> Result<(), SessionCleanupError> {
        let crate::ResolvedInputSource::Repository {
            repository_id,
            config,
            ..
        } = &self.input.source
        else {
            return Err(SessionCleanupError::InvalidRepositoryPublicationIntent(
                "publication input is not a Repository".into(),
            ));
        };
        if self.input.access != ResourceAccess::ReadWrite {
            return Err(SessionCleanupError::InvalidRepositoryPublicationIntent(
                "publication input is not writable".into(),
            ));
        }
        if config.repository_id != *repository_id {
            return Err(SessionCleanupError::InvalidRepositoryPublicationIntent(
                "Repository source and frozen config identities differ".into(),
            ));
        }
        self.expectation.validate().map_err(|error| {
            SessionCleanupError::InvalidRepositoryPublicationIntent(error.to_string())
        })
    }

    pub(super) fn repository_target(&self) -> Result<(&str, &str), SessionCleanupError> {
        self.validate()?;
        let crate::ResolvedInputSource::Repository {
            repository_id,
            config,
            ..
        } = &self.input.source
        else {
            unreachable!("validated publication intent is a Repository");
        };
        Ok((repository_id.as_str(), config.remote_url.as_str()))
    }
}

impl SessionRepositoryPublicationCommand {
    pub(super) fn new(
        session_id: &SessionId,
        cleanup_effect_id: &str,
        intent: &SessionRepositoryPublicationIntent,
    ) -> Result<Self, SessionCleanupError> {
        intent.validate()?;
        Ok(Self {
            session_id: session_id.clone(),
            effect_id: crate::stable_fingerprint(&(
                "session-repository-publication-effect-v1",
                session_id,
                cleanup_effect_id,
                intent,
            )),
            intent: intent.clone(),
        })
    }

    #[must_use]
    pub fn command_fingerprint(&self) -> String {
        crate::stable_fingerprint(&("session-repository-publication-command-v1", self))
    }
}

impl SessionRepositoryPublicationReceipt {
    #[must_use]
    pub fn new(
        command: &SessionRepositoryPublicationCommand,
        effect_receipt: RepositoryPublicationReceipt,
    ) -> Self {
        let command_fingerprint = command.command_fingerprint();
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-repository-publication-receipt-v1",
            command_fingerprint.as_str(),
            &effect_receipt,
        ));
        Self {
            command_fingerprint,
            effect_receipt,
            receipt_fingerprint,
        }
    }

    pub(super) fn verify(
        &self,
        command: &SessionRepositoryPublicationCommand,
    ) -> Result<(), SessionCleanupError> {
        self.verify_intent(&command.intent)?;
        if self.command_fingerprint != command.command_fingerprint() {
            return Err(SessionCleanupError::RepositoryPublicationReceiptMismatch);
        }
        Ok(())
    }

    fn verify_intent(
        &self,
        intent: &SessionRepositoryPublicationIntent,
    ) -> Result<(), SessionCleanupError> {
        let (repository_id, remote_url) = intent.repository_target()?;
        let canonical_fingerprint = crate::stable_fingerprint(&(
            "session-repository-publication-receipt-v1",
            self.command_fingerprint.as_str(),
            &self.effect_receipt,
        ));
        if self.effect_receipt.repository_id != repository_id
            || self.effect_receipt.source_remote_url != remote_url
            || self.effect_receipt.branch != intent.expectation.branch
            || self.effect_receipt.commit != intent.expectation.commit
            || self.receipt_fingerprint != canonical_fingerprint
        {
            return Err(SessionCleanupError::RepositoryPublicationReceiptMismatch);
        }
        Ok(())
    }
}

impl SessionRepositoryPublicationRejection {
    pub fn new(
        command: &SessionRepositoryPublicationCommand,
        effect_rejection: RepositoryPublicationRejection,
    ) -> Result<Self, SessionCleanupError> {
        Self::from_reason(
            command,
            SessionRepositoryPublicationRejectionReason::CompareAndSwap(effect_rejection),
        )
    }

    /// Record only a provider-validated closed observation of the exact source
    /// retained by the aggregate. Admission rechecks its binding before CAS.
    pub fn source_closed(
        command: &SessionRepositoryPublicationCommand,
        binding: &str,
        observation: awaken_provisioning_contract::SandboxObservation,
    ) -> Result<Self, SessionCleanupError> {
        if binding.trim().is_empty() {
            return Err(SessionCleanupError::RepositoryPublicationRejectionMismatch);
        }
        Self::from_reason(
            command,
            SessionRepositoryPublicationRejectionReason::SourceClosed {
                source_binding_fingerprint:
                    SessionRepositoryPublicationRejectionReason::binding_fingerprint(binding),
                observation,
            },
        )
    }

    fn from_reason(
        command: &SessionRepositoryPublicationCommand,
        effect_rejection: SessionRepositoryPublicationRejectionReason,
    ) -> Result<Self, SessionCleanupError> {
        command.intent.validate()?;
        effect_rejection.verify(&command.intent.expectation)?;
        let command_fingerprint = command.command_fingerprint();
        let rejection_fingerprint = crate::stable_fingerprint(&(
            "session-repository-publication-rejection-v1",
            command_fingerprint.as_str(),
            &effect_rejection,
        ));
        Ok(Self {
            command_fingerprint,
            effect_rejection,
            rejection_fingerprint,
        })
    }

    pub(super) fn verify(
        &self,
        command: &SessionRepositoryPublicationCommand,
    ) -> Result<(), SessionCleanupError> {
        self.verify_intent(&command.intent)?;
        if self.command_fingerprint != command.command_fingerprint() {
            return Err(SessionCleanupError::RepositoryPublicationRejectionMismatch);
        }
        Ok(())
    }

    fn verify_intent(
        &self,
        intent: &SessionRepositoryPublicationIntent,
    ) -> Result<(), SessionCleanupError> {
        self.effect_rejection.verify(&intent.expectation)?;
        let canonical_fingerprint = crate::stable_fingerprint(&(
            "session-repository-publication-rejection-v1",
            self.command_fingerprint.as_str(),
            &self.effect_rejection,
        ));
        if self.rejection_fingerprint != canonical_fingerprint {
            return Err(SessionCleanupError::RepositoryPublicationRejectionMismatch);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for SessionRepositoryPublicationCleanup {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(deserialize_with = "super::state::deserialize_persisted_operation")]
            cleanup: SessionCleanupOperation,
            intent: SessionRepositoryPublicationIntent,
            #[serde(default)]
            receipt: Option<SessionRepositoryPublicationReceipt>,
            #[serde(default)]
            rejection: Option<SessionRepositoryPublicationRejection>,
        }

        let wire = Wire::deserialize(deserializer)?;
        wire.intent.validate().map_err(serde::de::Error::custom)?;
        if let Some(receipt) = wire.receipt.as_ref() {
            receipt
                .verify_intent(&wire.intent)
                .map_err(serde::de::Error::custom)?;
        }
        if let Some(rejection) = wire.rejection.as_ref() {
            // The Session id contributes to the command fingerprint and is not
            // available inside this private sidecar decoder. Full command binding
            // is therefore reverified by every aggregate accessor/mutation; the
            // decoder still admits only the canonical rejection fingerprint for
            // the frozen intent and stored command fingerprint.
            rejection
                .verify_intent(&wire.intent)
                .map_err(serde::de::Error::custom)?;
        }
        if wire.receipt.is_some() && wire.rejection.is_some() {
            return Err(serde::de::Error::custom(
                "Repository publication cannot contain both a receipt and a rejection",
            ));
        }
        match (wire.cleanup.state(), &wire.receipt, &wire.rejection) {
            (SessionCleanupState::NotRequested, _, _) => {
                return Err(serde::de::Error::custom(
                    "Repository publication cannot wrap an unrequested cleanup",
                ));
            }
            (SessionCleanupState::RepositoryPublication(_), _, _) => {
                return Err(serde::de::Error::custom(
                    "nested Repository publication cleanup is forbidden",
                ));
            }
            (SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_), _, _) => {
                return Err(serde::de::Error::custom(
                    "cleanup progress must be the outermost operation wrapper",
                ));
            }
            (SessionCleanupState::Fenced { .. }, Some(_), _)
            | (SessionCleanupState::Fenced { .. }, _, Some(_)) => {
                return Err(serde::de::Error::custom(
                    "a fenced cleanup cannot have a Repository publication outcome",
                ));
            }
            (SessionCleanupState::Completed { .. }, None, None) => {
                return Err(serde::de::Error::custom(
                    "a completed publication cleanup requires its exact outcome",
                ));
            }
            (
                SessionCleanupState::Fenced { .. }
                | SessionCleanupState::Requested { .. }
                | SessionCleanupState::Completed { .. },
                _,
                _,
            ) => {}
        }
        Ok(Self {
            cleanup: wire.cleanup,
            intent: wire.intent,
            receipt: wire.receipt,
            rejection: wire.rejection,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::completion::SessionCleanupCompletion;
    use super::*;
    use awaken_provisioning_contract::RepositoryPublicationExpectation;
    use awaken_resource_contract::ResourceAccess;

    #[derive(serde::Deserialize)]
    struct PersistedCleanupFixture {
        #[serde(deserialize_with = "super::super::state::deserialize_persisted_operation")]
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

    fn intent() -> SessionRepositoryPublicationIntent {
        SessionRepositoryPublicationIntent {
            input: crate::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::from("source"),
                source: crate::ResolvedInputSource::Repository {
                    repository_id: awaken_resource_contract::RepositoryId::from("repo-1"),
                    config: awaken_resource_contract::RepositoryConfigVersion {
                        repository_id: awaken_resource_contract::RepositoryId::from("repo-1"),
                        version: awaken_resource_contract::ConfigVersion(7),
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

    fn pending(session_id: &str) -> (SessionCleanupOperation, SessionRepositoryPublicationCommand) {
        let mut state = SessionCleanupOperation::default();
        state
            .request_with_publication(&crate::SessionId::from(session_id), intent())
            .unwrap();
        state
            .freeze_targets(&crate::SessionId::from(session_id), [], 3, 5)
            .unwrap();
        let command = state
            .publication_command(&crate::SessionId::from(session_id))
            .unwrap()
            .expect("publication command");
        (state, command)
    }

    fn rejected(
        session_id: &str,
    ) -> (
        SessionCleanupOperation,
        SessionRepositoryPublicationCommand,
        SessionRepositoryPublicationRejection,
    ) {
        let (mut state, command) = pending(session_id);
        let rejection = SessionRepositoryPublicationRejection::new(
            &command,
            RepositoryPublicationRejection::RemoteRefAbsent,
        )
        .unwrap();
        state
            .record_repository_publication_rejection(
                &crate::SessionId::from(session_id),
                rejection.clone(),
            )
            .unwrap();
        (state, command, rejection)
    }

    fn receipt(
        command: &SessionRepositoryPublicationCommand,
    ) -> SessionRepositoryPublicationReceipt {
        let (repository_id, remote_url) = command.intent.repository_target().unwrap();
        SessionRepositoryPublicationReceipt::new(
            command,
            RepositoryPublicationReceipt {
                repository_id: repository_id.into(),
                source_remote_url: remote_url.into(),
                branch: command.intent.expectation.branch.clone(),
                commit: command.intent.expectation.commit.clone(),
            },
        )
    }

    #[test]
    fn publication_negative_evidence_keeps_legacy_cas_wire_and_rejects_unproven_closure() {
        // Causes: C1 CAS/source closure; C2 exact/nonempty source identity;
        // C3 absent/terminal versus ready/provisioning/disposing/foreign facts;
        // C4 canonical/tampered evidence. Effects: N1 legacy JSON and v1 hash
        // stay exact; N2 only closed observations produce negative evidence;
        // N3 no success/remote-CAS claim is derived from closure; N4 tampering
        // is rejected before work suppression. Provider validation is a Worker
        // prerequisite; this table proves wire/aggregate admission only.
        let (_, command) = pending("source-evidence");
        let cas = RepositoryPublicationRejection::RemoteRefAbsent;
        let legacy_hash = crate::stable_fingerprint(&(
            "session-repository-publication-rejection-v1",
            command.command_fingerprint().as_str(),
            &cas,
        ));
        let rejection = SessionRepositoryPublicationRejection::new(&command, cas.clone()).unwrap();
        assert_eq!(rejection.rejection_fingerprint, legacy_hash, "N1 old hash");
        assert_eq!(
            serde_json::to_value(&rejection).unwrap()["effect_rejection"],
            serde_json::to_value(cas).unwrap(),
            "N1 original transport wire"
        );
        assert_eq!(
            serde_json::from_value::<SessionRepositoryPublicationRejection>(
                serde_json::to_value(&rejection).unwrap()
            )
            .unwrap(),
            rejection,
            "N1 unchanged decoder"
        );
        use awaken_provisioning_contract::SandboxObservation as O;
        for (observation, accepts) in [
            (
                O::DefinitivelyUnavailable {
                    physical_incarnation: None,
                },
                true,
            ),
            (
                O::DefinitivelyUnavailable {
                    physical_incarnation: Some("pod-a".into()),
                },
                true,
            ),
            (
                O::Terminal {
                    physical_incarnation: "pod-a".into(),
                },
                true,
            ),
            (
                O::Terminal {
                    physical_incarnation: " ".into(),
                },
                false,
            ),
            (
                O::DefinitivelyUnavailable {
                    physical_incarnation: Some(String::new()),
                },
                false,
            ),
            (O::Ready, false),
            (O::Provisioning { deadline: None }, false),
            (
                O::Disposing {
                    physical_incarnation: "pod-a".into(),
                },
                false,
            ),
            (
                O::Incompatible {
                    reason: "foreign".into(),
                },
                false,
            ),
        ] {
            let result = SessionRepositoryPublicationRejection::source_closed(
                &command,
                "source-a",
                observation.clone(),
            );
            assert_eq!(result.is_ok(), accepts, "N2 {observation:?}");
            if let Ok(rejection) = result {
                assert!(
                    rejection.effect_rejection.is_source_closed(),
                    "N3 distinct cause"
                );
                assert!(rejection.effect_rejection.to_string().contains("unknown"));
                rejection.verify(&command).unwrap();
                let mut changed = rejection.clone();
                changed.rejection_fingerprint.push('x');
                assert!(changed.verify(&command).is_err(), "N4 tampered closure");
            }
        }
        for binding in ["", " "] {
            assert!(
                SessionRepositoryPublicationRejection::source_closed(
                    &command,
                    binding,
                    O::DefinitivelyUnavailable {
                        physical_incarnation: None
                    },
                )
                .is_err(),
                "N2 source identity required"
            );
        }
    }

    #[test]
    fn local_head_conflict_replays_through_the_existing_terminal_sidecar() {
        // Causes: L1 canonical local HEAD differs from the frozen output;
        // L2 identical command/rejection is replayed; L3 evidence changes to
        // the promised commit. Effects: E1 one Session-root negative outcome,
        // E2 no second mutation, E3 invalid evidence refused. The local
        // observation makes no claim about the remote branch or prior push.
        let session_id = crate::SessionId::from("local-head-conflict");
        let (mut state, command) = pending(session_id.as_str());
        let rejection = SessionRepositoryPublicationRejection::new(
            &command,
            RepositoryPublicationRejection::LocalCommitMismatch {
                observed_commit: "2".repeat(40),
            },
        )
        .expect("L1 exact local observation");
        let wire = serde_json::to_value(&rejection).unwrap();
        assert_eq!(
            serde_json::from_value::<SessionRepositoryPublicationRejection>(wire).unwrap(),
            rejection,
            "L1 stable sidecar wire"
        );
        assert!(
            state
                .record_repository_publication_rejection(&session_id, rejection.clone())
                .unwrap(),
            "E1 first root effect"
        );
        assert!(
            !state
                .record_repository_publication_rejection(&session_id, rejection.clone())
                .unwrap(),
            "E2 exact replay"
        );
        assert_eq!(
            state.repository_publication_rejection(&session_id).unwrap(),
            Some(&rejection),
            "E1 one retained negative outcome"
        );
        let mut tampered = rejection;
        tampered.effect_rejection = SessionRepositoryPublicationRejectionReason::CompareAndSwap(
            RepositoryPublicationRejection::LocalCommitMismatch {
                observed_commit: command.intent.expectation.commit.clone(),
            },
        );
        assert!(tampered.verify(&command).is_err(), "E3");
    }

    fn complete(mut state: SessionCleanupOperation, session_id: &str) -> SessionCleanupOperation {
        let root = state
            .command_for(&crate::SessionId::from(session_id), session_id)
            .unwrap();
        let completion = SessionCleanupCompletion::new(&root, Vec::new());
        let mut wire = serde_json::to_value(&state).unwrap();
        wire["cleanup"]
            .as_object_mut()
            .expect("publication cleanup wire")
            .entry("completions")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .expect("legacy completion map")
            .insert(
                root.thread_id.clone(),
                serde_json::to_value(completion).unwrap(),
            );
        state = decode_persisted_cleanup(wire).unwrap();
        let receipts = state.recorded_receipts(session_id).unwrap();
        state.complete(session_id, &receipts).unwrap();
        state
    }

    /// Decoder/aggregate cause-effect design: C1 outcome is receipt, rejection,
    /// both, or absent; C2 its intent-local fingerprint is canonical or forged;
    /// C3 its outer Session command is exact or foreign; C4 the phase is pending
    /// or Completed. Effects: E1 admit one canonical outcome; E2 reject dual,
    /// absent-Completed, or forged evidence at decode; E3 reject foreign command
    /// binding before mutation, work suppression, or tombstone admission.
    ///
    /// | Rule | outcome | local fingerprint | outer command | phase | Effect |
    /// | D1 | rejection | forged | any | pending | E2 |
    /// | D2 | both | canonical | any | pending | E2 |
    /// | D3 | rejection | canonical | foreign | pending | E3 |
    /// | D4 | receipt | canonical | foreign | Completed | E3 |
    /// | D5 | rejection | canonical | foreign | Completed | E3 |
    #[test]
    fn rejection_decoder_rejects_a_tampered_fingerprint() {
        let (state, _, _) = rejected("decoder-fingerprint");
        let mut wire = serde_json::to_value(state).unwrap();
        wire["rejection"]["rejection_fingerprint"] = serde_json::json!("forged");
        assert!(
            decode_persisted_cleanup(wire).is_err(),
            "D1 tampered rejection fingerprint fails before aggregate use"
        );
    }

    #[test]
    fn rejection_decoder_rejects_a_receipt_and_rejection_pair() {
        let (state, command, _) = rejected("decoder-conflict");
        let mut wire = serde_json::to_value(state).unwrap();
        wire["receipt"] = serde_json::to_value(receipt(&command)).unwrap();
        assert!(
            decode_persisted_cleanup(wire).is_err(),
            "D2 one sidecar cannot admit both terminal outcomes"
        );
    }

    #[test]
    fn rejection_command_mismatch_leaves_the_aggregate_unchanged() {
        let (foreign_state, _, foreign) = rejected("foreign-session");
        foreign_state
            .verify_for(&crate::SessionId::from("foreign-session"))
            .unwrap();
        let (mut state, _) = pending("target-session");
        let before = state.clone();
        assert_eq!(
            state.record_repository_publication_rejection(
                &crate::SessionId::from("target-session"),
                foreign
            ),
            Err(SessionCleanupError::RepositoryPublicationRejectionMismatch),
            "D3 canonical self-evidence still requires the exact Session command"
        );
        assert_eq!(state, before, "D3 rejection precedes mutation");
    }

    #[test]
    fn receipt_command_mismatch_is_never_exposed_or_recorded() {
        let (_, foreign_command) = pending("foreign-receipt-session");
        let foreign = receipt(&foreign_command);
        let (mut state, _) = pending("target-receipt-session");
        let before = state.clone();
        assert_eq!(
            state.record_repository_publication_receipt(
                &crate::SessionId::from("target-receipt-session"),
                foreign.clone(),
            ),
            Err(SessionCleanupError::RepositoryPublicationReceiptMismatch),
            "D4 a receipt for another Session is rejected before mutation"
        );
        assert_eq!(state, before, "D4 mutation is unchanged");

        // Intent-local decoding cannot reconstruct the outer Session id. Even a
        // self-consistent foreign fingerprint in a Completed phase is therefore
        // re-bound before it can suppress cleanup or authorize a tombstone.
        let own_receipt = receipt(&pending("target-receipt-session").1);
        let mut completed = before.clone();
        completed
            .record_repository_publication_receipt(
                &crate::SessionId::from("target-receipt-session"),
                own_receipt,
            )
            .unwrap();
        let completed = complete(completed, "target-receipt-session");
        let mut wire = serde_json::to_value(completed).unwrap();
        wire["receipt"] = serde_json::to_value(foreign).unwrap();
        let decoded = decode_persisted_cleanup(wire)
            .expect("D4 intent-local receipt shape remains decodable");
        assert!(
            decoded.is_completed(),
            "D4 setup reaches the dangerous phase"
        );
        assert_eq!(
            decoded.verify_for(&crate::SessionId::from("target-receipt-session")),
            Err(SessionCleanupError::RepositoryPublicationReceiptMismatch),
            "D4 Completed suppression requires outer Session binding"
        );
        assert_eq!(
            decoded
                .repository_publication_receipt(&crate::SessionId::from("target-receipt-session")),
            Err(SessionCleanupError::RepositoryPublicationReceiptMismatch),
            "D4 exact Session command binding is mandatory at replay"
        );
    }

    #[test]
    fn foreign_completed_rejection_cannot_suppress_cleanup() {
        let (_, _, foreign) = rejected("foreign-rejection-session");
        let (target, _, _) = rejected("target-rejection-session");
        let completed = complete(target, "target-rejection-session");
        let mut wire = serde_json::to_value(completed).unwrap();
        wire["rejection"] = serde_json::to_value(foreign).unwrap();
        let decoded = decode_persisted_cleanup(wire)
            .expect("D5 intent-local rejection remains structurally decodable");
        assert!(
            decoded.is_completed(),
            "D5 setup reaches the dangerous phase"
        );
        assert_eq!(
            decoded.verify_for(&crate::SessionId::from("target-rejection-session")),
            Err(SessionCleanupError::RepositoryPublicationRejectionMismatch),
            "D5 Completed rejection requires outer Session binding"
        );
    }
}
