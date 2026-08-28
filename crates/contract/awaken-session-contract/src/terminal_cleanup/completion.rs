//! Exact per-thread completion evidence for the terminal cleanup operation.
//!
//! This module owns receipt canonicalization only. Cleanup phase authority and
//! aggregate transitions remain in the parent module.

use super::{SessionCleanupCommand, SessionCleanupError, session_cleanup_completion_admitted};
use awaken_resource_contract::ArtifactPublicationReceipt;
use serde::{Deserialize, Serialize};

/// Untrusted Runtime report that one per-thread cleanup command completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCleanupCompletion {
    pub session_id: String,
    pub thread_id: String,
    pub effect_id: String,
    pub artifact_receipts: Vec<ArtifactPublicationReceipt>,
    pub receipt_fingerprint: String,
}

impl SessionCleanupCompletion {
    #[must_use]
    pub fn new(
        command: &SessionCleanupCommand,
        mut artifact_receipts: Vec<ArtifactPublicationReceipt>,
    ) -> Self {
        artifact_receipts.sort_by(|left, right| left.effect_id.cmp(&right.effect_id));
        let artifact_evidence = artifact_receipts
            .iter()
            .map(|receipt| (receipt.effect_id.as_str(), receipt.content_id.as_str()))
            .collect::<Vec<_>>();
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-thread-receipt-v1",
            command.session_id.as_str(),
            command.thread_id.as_str(),
            command.effect_id.as_str(),
            artifact_evidence,
        ));
        Self {
            session_id: command.session_id.clone(),
            thread_id: command.thread_id.clone(),
            effect_id: command.effect_id.clone(),
            artifact_receipts,
            receipt_fingerprint,
        }
    }

    pub fn verify(
        &self,
        command: &SessionCleanupCommand,
    ) -> Result<VerifiedSessionCleanupReceipt, SessionCleanupError> {
        let duplicate_artifact = self
            .artifact_receipts
            .windows(2)
            .any(|pair| pair[0].effect_id == pair[1].effect_id);
        let canonical = Self::new(command, self.artifact_receipts.clone());
        if !session_cleanup_completion_admitted(
            !duplicate_artifact,
            self.session_id == command.session_id,
            self.thread_id == command.thread_id,
            self.effect_id == command.effect_id,
            *self == canonical,
        ) {
            return Err(SessionCleanupError::ReceiptMismatch);
        }
        Ok(VerifiedSessionCleanupReceipt {
            command: command.clone(),
            completion: self.clone(),
        })
    }
}

/// Exact, process-local completion evidence admitted against one cleanup
/// command. It is intentionally not serializable and has no public constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSessionCleanupReceipt {
    command: SessionCleanupCommand,
    completion: SessionCleanupCompletion,
}

impl VerifiedSessionCleanupReceipt {
    #[must_use]
    pub fn command(&self) -> &SessionCleanupCommand {
        &self.command
    }

    #[must_use]
    pub fn completion(&self) -> &SessionCleanupCompletion {
        &self.completion
    }

    pub(super) fn thread_id(&self) -> &str {
        &self.completion.thread_id
    }

    pub(super) fn effect_id(&self) -> &str {
        &self.completion.effect_id
    }

    pub(super) fn receipt_fingerprint(&self) -> &str {
        &self.completion.receipt_fingerprint
    }
}
