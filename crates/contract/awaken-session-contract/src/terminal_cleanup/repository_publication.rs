//! Exact Repository publication effect vocabulary and sidecar codec.
//!
//! This module owns the secret-free intent, command, receipt verification, and
//! one-level cleanup wrapper. Aggregate archive admission remains solely in
//! `session_repo::repository_publication`; the generic cleanup phase machine
//! remains solely in the parent module.

use super::{
    SessionCleanupError, SessionCleanupOperation, SessionRepositoryPublicationCleanup,
    SessionRepositoryPublicationCommand, SessionRepositoryPublicationIntent,
    SessionRepositoryPublicationReceipt,
};
use awaken_provisioning_contract::RepositoryPublicationReceipt;
use awaken_resource_contract::ResourceAccess;
use serde::Deserialize;

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
        }

        let wire = Wire::deserialize(deserializer)?;
        wire.intent.validate().map_err(serde::de::Error::custom)?;
        if let Some(receipt) = wire.receipt.as_ref() {
            receipt
                .verify_intent(&wire.intent)
                .map_err(serde::de::Error::custom)?;
        }
        match (&wire.cleanup, &wire.receipt) {
            (SessionCleanupOperation::NotRequested, _) => {
                return Err(serde::de::Error::custom(
                    "Repository publication cannot wrap an unrequested cleanup",
                ));
            }
            (SessionCleanupOperation::RepositoryPublication(_), _) => {
                return Err(serde::de::Error::custom(
                    "nested Repository publication cleanup is forbidden",
                ));
            }
            (SessionCleanupOperation::Fenced { .. }, Some(_)) => {
                return Err(serde::de::Error::custom(
                    "a fenced cleanup cannot have a Repository publication receipt",
                ));
            }
            (SessionCleanupOperation::Completed { .. }, None) => {
                return Err(serde::de::Error::custom(
                    "a completed publication cleanup requires its exact receipt",
                ));
            }
            (
                SessionCleanupOperation::Fenced { .. }
                | SessionCleanupOperation::Requested { .. }
                | SessionCleanupOperation::Completed { .. },
                _,
            ) => {}
        }
        Ok(Self {
            cleanup: wire.cleanup,
            intent: wire.intent,
            receipt: wire.receipt,
        })
    }
}
