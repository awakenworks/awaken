//! Exact Repository publication effect vocabulary and sidecar codec.
//!
//! This module owns the secret-free intent, command, receipt verification, and
//! one-level cleanup wrapper. Aggregate archive admission remains solely in
//! `session_repo::repository_publication`; the generic cleanup phase machine
//! remains solely in the parent module.

use super::{
    SessionCleanupError, SessionCleanupOperation, SessionRepositoryPublicationCleanup,
    SessionRepositoryPublicationCommand, SessionRepositoryPublicationEffect,
    SessionRepositoryPublicationIntent, SessionRepositoryPublicationReceipt,
    SessionRepositoryPublicationRejection, cleanup_effect_id,
};
use awaken_provisioning_contract::{RepositoryPublicationReceipt, RepositoryPublicationRejection};
use awaken_resource_contract::ResourceAccess;
use serde::Deserialize;

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

impl SessionCleanupOperation {
    /// Rebind a persisted publication sidecar to its outer Session identity.
    ///
    /// The sidecar decoder can verify intent-local fingerprints, but the
    /// Session id is deliberately owned by [`crate::PersistedSession`]. Every
    /// aggregate/store/application boundary must call this method before a
    /// Completed phase can suppress work or authorize a tombstone.
    pub fn verify_for(&self, session_id: &str) -> Result<(), SessionCleanupError> {
        let Self::RepositoryPublication(publication) = self else {
            return Ok(());
        };
        publication.intent.validate()?;
        let expected_effect_id = cleanup_effect_id(session_id);
        match &publication.cleanup {
            Self::Fenced { effect_id } => {
                if effect_id != &expected_effect_id {
                    return Err(SessionCleanupError::OperationMismatch);
                }
                if publication.receipt.is_some() || publication.rejection.is_some() {
                    return Err(SessionCleanupError::RepositoryPublicationOutcomeMismatch);
                }
            }
            Self::Requested {
                effect_id,
                thread_ids,
                ..
            } => {
                if effect_id != &expected_effect_id {
                    return Err(SessionCleanupError::OperationMismatch);
                }
                if !thread_ids.contains(session_id) {
                    return Err(SessionCleanupError::FrozenTargetsMismatch);
                }
                if publication.receipt.is_some() || publication.rejection.is_some() {
                    let command = SessionRepositoryPublicationCommand::new(
                        session_id,
                        effect_id,
                        &publication.intent,
                    )?;
                    verified_repository_publication_outcome(publication, &command)?;
                }
            }
            Self::Completed {
                effect_id,
                thread_ids,
                ..
            } => {
                if effect_id != &expected_effect_id {
                    return Err(SessionCleanupError::OperationMismatch);
                }
                if !thread_ids.contains(session_id) {
                    return Err(SessionCleanupError::FrozenTargetsMismatch);
                }
                let command = SessionRepositoryPublicationCommand::new(
                    session_id,
                    effect_id,
                    &publication.intent,
                )?;
                verified_repository_publication_outcome(publication, &command)?;
            }
            Self::NotRequested | Self::RepositoryPublication(_) => {
                return Err(SessionCleanupError::FrozenRepositoryPublicationMismatch);
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn repository_publication_intent(&self) -> Option<&SessionRepositoryPublicationIntent> {
        match self {
            Self::RepositoryPublication(publication) => Some(&publication.intent),
            Self::NotRequested
            | Self::Fenced { .. }
            | Self::Requested { .. }
            | Self::Completed { .. } => None,
        }
    }

    /// Project the one root Repository publication effect only after every
    /// child Runtime cleanup receipt is durable and before the root cleanup is
    /// allowed to dispose the shared environment.
    pub fn publication_command(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionRepositoryPublicationCommand>, SessionCleanupError> {
        self.verify_for(session_id)?;
        let Self::RepositoryPublication(publication) = self else {
            return match self {
                Self::Requested { .. } | Self::Completed { .. } => Ok(None),
                Self::NotRequested | Self::Fenced { .. } => Err(SessionCleanupError::NotRequested),
                Self::RepositoryPublication(_) => unreachable!(),
            };
        };
        let Self::Requested {
            effect_id,
            thread_ids,
            completions,
            ..
        } = &publication.cleanup
        else {
            return if publication.cleanup.is_completed() {
                Ok(None)
            } else {
                Err(SessionCleanupError::NotRequested)
            };
        };
        if *effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        if !thread_ids.contains(session_id) {
            return Err(SessionCleanupError::FrozenTargetsMismatch);
        }
        if thread_ids
            .iter()
            .any(|thread_id| thread_id != session_id && !completions.contains_key(thread_id))
        {
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
        session_id: &str,
    ) -> Result<Option<&SessionRepositoryPublicationReceipt>, SessionCleanupError> {
        self.verify_for(session_id)?;
        match self {
            Self::RepositoryPublication(publication) => {
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
                let command = SessionRepositoryPublicationCommand::new(
                    session_id,
                    effect_id,
                    &publication.intent,
                )?;
                receipt.verify(&command)?;
                Ok(Some(receipt))
            }
            Self::NotRequested
            | Self::Fenced { .. }
            | Self::Requested { .. }
            | Self::Completed { .. } => Ok(None),
        }
    }

    pub fn repository_publication_rejection(
        &self,
        session_id: &str,
    ) -> Result<Option<&SessionRepositoryPublicationRejection>, SessionCleanupError> {
        self.verify_for(session_id)?;
        match self {
            Self::RepositoryPublication(publication) => {
                let Some(rejection) = publication.rejection.as_ref() else {
                    return Ok(None);
                };
                let effect_id = publication
                    .cleanup
                    .effect_id()
                    .ok_or(SessionCleanupError::NotRequested)?;
                let command = SessionRepositoryPublicationCommand::new(
                    session_id,
                    effect_id,
                    &publication.intent,
                )?;
                rejection.verify(&command)?;
                Ok(Some(rejection))
            }
            Self::NotRequested
            | Self::Fenced { .. }
            | Self::Requested { .. }
            | Self::Completed { .. } => Ok(None),
        }
    }

    /// Verify and durably retain the exact root Repository publication receipt.
    /// Children must already be complete. Exact replay is a no-op, while a
    /// different command binding or effect receipt fails closed.
    pub fn record_repository_publication_receipt(
        &mut self,
        session_id: &str,
        receipt: SessionRepositoryPublicationReceipt,
    ) -> Result<bool, SessionCleanupError> {
        self.record_repository_publication_effect(
            session_id,
            SessionRepositoryPublicationEffect::Published(receipt),
        )
    }

    /// Verify and retain one permanent compare-and-swap rejection in the same
    /// sidecar as a successful publication receipt. Once durable, the ordinary
    /// root cleanup command may run; exact replay cannot invoke Git again.
    pub fn record_repository_publication_rejection(
        &mut self,
        session_id: &str,
        rejection: SessionRepositoryPublicationRejection,
    ) -> Result<bool, SessionCleanupError> {
        self.record_repository_publication_effect(
            session_id,
            SessionRepositoryPublicationEffect::Rejected(rejection),
        )
    }

    fn record_repository_publication_effect(
        &mut self,
        session_id: &str,
        effect: SessionRepositoryPublicationEffect,
    ) -> Result<bool, SessionCleanupError> {
        let Self::RepositoryPublication(publication) = self else {
            return match self {
                Self::Fenced { .. } => Err(SessionCleanupError::RepositoryPublicationNotReady),
                Self::NotRequested | Self::Requested { .. } | Self::Completed { .. } => {
                    Err(SessionCleanupError::RepositoryPublicationNotRequested)
                }
                Self::RepositoryPublication(_) => unreachable!(),
            };
        };
        let publication = publication.as_mut();
        let (effect_id, root_present, children_complete, completed) = match &publication.cleanup {
            Self::Requested {
                effect_id,
                thread_ids,
                completions,
                ..
            } => (
                effect_id.clone(),
                thread_ids.contains(session_id),
                thread_ids.iter().all(|thread_id| {
                    thread_id == session_id || completions.contains_key(thread_id)
                }),
                false,
            ),
            Self::Completed { effect_id, .. } => (effect_id.clone(), true, true, true),
            Self::NotRequested => {
                return Err(SessionCleanupError::RepositoryPublicationNotRequested);
            }
            Self::Fenced { .. } => {
                return Err(SessionCleanupError::RepositoryPublicationNotReady);
            }
            Self::RepositoryPublication(_) => {
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
        session_id: &str,
        cleanup_effect_id: &str,
        intent: &SessionRepositoryPublicationIntent,
    ) -> Result<Self, SessionCleanupError> {
        intent.validate()?;
        Ok(Self {
            session_id: session_id.to_string(),
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
        effect_rejection
            .verify(&command.intent.expectation)
            .map_err(|_| SessionCleanupError::RepositoryPublicationRejectionMismatch)?;
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
        self.effect_rejection
            .verify(&intent.expectation)
            .map_err(|_| SessionCleanupError::RepositoryPublicationRejectionMismatch)?;
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
        match (&wire.cleanup, &wire.receipt, &wire.rejection) {
            (SessionCleanupOperation::NotRequested, _, _) => {
                return Err(serde::de::Error::custom(
                    "Repository publication cannot wrap an unrequested cleanup",
                ));
            }
            (SessionCleanupOperation::RepositoryPublication(_), _, _) => {
                return Err(serde::de::Error::custom(
                    "nested Repository publication cleanup is forbidden",
                ));
            }
            (SessionCleanupOperation::Fenced { .. }, Some(_), _)
            | (SessionCleanupOperation::Fenced { .. }, _, Some(_)) => {
                return Err(serde::de::Error::custom(
                    "a fenced cleanup cannot have a Repository publication outcome",
                ));
            }
            (SessionCleanupOperation::Completed { .. }, None, None) => {
                return Err(serde::de::Error::custom(
                    "a completed publication cleanup requires its exact outcome",
                ));
            }
            (
                SessionCleanupOperation::Fenced { .. }
                | SessionCleanupOperation::Requested { .. }
                | SessionCleanupOperation::Completed { .. },
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
    use super::*;
    use awaken_provisioning_contract::RepositoryPublicationExpectation;
    use awaken_resource_contract::ResourceAccess;

    fn intent() -> SessionRepositoryPublicationIntent {
        SessionRepositoryPublicationIntent {
            input: crate::ResolvedInput {
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
            .request_with_publication(session_id, intent())
            .unwrap();
        state.freeze_targets(session_id, [], 3, 5).unwrap();
        let command = state
            .publication_command(session_id)
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
            .record_repository_publication_rejection(session_id, rejection.clone())
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

    fn complete(mut state: SessionCleanupOperation, session_id: &str) -> SessionCleanupOperation {
        let root = state.command_for(session_id, session_id).unwrap();
        state
            .record_completion(
                session_id,
                crate::SessionCleanupCompletion::new(&root, Vec::new()),
            )
            .unwrap();
        let receipts = state.recorded_receipts(session_id).unwrap();
        state.complete(session_id, &receipts).unwrap();
        state
    }

    /// Decoder/aggregate cause-effect table: malformed self-evidence and dual
    /// outcomes fail at decode, while a canonical outcome bound to another
    /// Session fails at aggregate admission without changing durable state.
    #[test]
    fn rejection_decoder_rejects_a_tampered_fingerprint() {
        let (state, _, _) = rejected("decoder-fingerprint");
        let mut wire = serde_json::to_value(state).unwrap();
        wire["rejection"]["rejection_fingerprint"] = serde_json::json!("forged");
        assert!(
            serde_json::from_value::<SessionCleanupOperation>(wire).is_err(),
            "D1 tampered rejection fingerprint fails before aggregate use"
        );
    }

    #[test]
    fn rejection_decoder_rejects_a_receipt_and_rejection_pair() {
        let (state, command, _) = rejected("decoder-conflict");
        let mut wire = serde_json::to_value(state).unwrap();
        wire["receipt"] = serde_json::to_value(receipt(&command)).unwrap();
        assert!(
            serde_json::from_value::<SessionCleanupOperation>(wire).is_err(),
            "D2 one sidecar cannot admit both terminal outcomes"
        );
    }

    #[test]
    fn rejection_command_mismatch_leaves_the_aggregate_unchanged() {
        let (foreign_state, _, foreign) = rejected("foreign-session");
        foreign_state.verify_for("foreign-session").unwrap();
        let (mut state, _) = pending("target-session");
        let before = state.clone();
        assert_eq!(
            state.record_repository_publication_rejection("target-session", foreign),
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
            state.record_repository_publication_receipt("target-receipt-session", foreign.clone(),),
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
            .record_repository_publication_receipt("target-receipt-session", own_receipt)
            .unwrap();
        let completed = complete(completed, "target-receipt-session");
        let mut wire = serde_json::to_value(completed).unwrap();
        wire["receipt"] = serde_json::to_value(foreign).unwrap();
        let decoded: SessionCleanupOperation =
            serde_json::from_value(wire).expect("D4 intent-local receipt shape remains decodable");
        assert!(
            decoded.is_completed(),
            "D4 setup reaches the dangerous phase"
        );
        assert_eq!(
            decoded.verify_for("target-receipt-session"),
            Err(SessionCleanupError::RepositoryPublicationReceiptMismatch),
            "D4 Completed suppression requires outer Session binding"
        );
        assert_eq!(
            decoded.repository_publication_receipt("target-receipt-session"),
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
        let decoded: SessionCleanupOperation = serde_json::from_value(wire)
            .expect("D5 intent-local rejection remains structurally decodable");
        assert!(
            decoded.is_completed(),
            "D5 setup reaches the dangerous phase"
        );
        assert_eq!(
            decoded.verify_for("target-rejection-session"),
            Err(SessionCleanupError::RepositoryPublicationRejectionMismatch),
            "D5 Completed rejection requires outer Session binding"
        );
    }
}
