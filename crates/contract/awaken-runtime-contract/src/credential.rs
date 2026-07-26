//! Credential execution contracts.
//!
//! Published policy and material-resolution values live in
//! `awaken-credential-contract`. Attempt-scoped realization facts live here
//! because every execution backend consumes them through `RuntimeRunContext`.
//! Dispatch owns their persistence, but Native and ACP share this one neutral
//! vocabulary and recorder port.

pub use awaken_credential_contract::*;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::resolved::ResolvedModelCandidate;

/// Content identity of one complete published model candidate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CandidateFingerprint(pub String);

/// One credential execution decision frozen under an exact dispatch claim
/// epoch. Publication-pinned fallback candidates receive separate entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptCredentialBinding {
    pub candidate_fingerprint: CandidateFingerprint,
    pub credential: CredentialRef,
    pub selected_plaintext_holder: PlaintextHolder,
    pub selected_realization_kind: CredentialRealizationKind,
    pub claim_epoch: u64,
}

/// Secret-free evidence that one exact attempt binding was realized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRealizationReceipt {
    pub candidate_fingerprint: CandidateFingerprint,
    pub credential: CredentialRef,
    pub selected_plaintext_holder: PlaintextHolder,
    pub actual_realization_kind: CredentialRealizationKind,
    pub claim_epoch: u64,
    pub receipt_fingerprint: String,
}

impl CredentialRealizationReceipt {
    /// Construct a receipt for the effect actually completed. Verification
    /// rejects it unless that effect equals the frozen attempt decision.
    pub fn new(
        binding: &AttemptCredentialBinding,
        actual_realization_kind: CredentialRealizationKind,
    ) -> Result<Self, CredentialReceiptError> {
        let mut receipt = Self {
            candidate_fingerprint: binding.candidate_fingerprint.clone(),
            credential: binding.credential.clone(),
            selected_plaintext_holder: binding.selected_plaintext_holder.clone(),
            actual_realization_kind,
            claim_epoch: binding.claim_epoch,
            receipt_fingerprint: String::new(),
        };
        receipt.receipt_fingerprint = receipt.expected_fingerprint()?;
        receipt.verify(binding)?;
        Ok(receipt)
    }

    fn expected_fingerprint(&self) -> Result<String, CredentialReceiptError> {
        crate::resolution::content_fingerprint(&(
            &self.candidate_fingerprint,
            &self.credential,
            &self.selected_plaintext_holder,
            self.actual_realization_kind,
            self.claim_epoch,
        ))
        .map(|fingerprint| format!("sha256:{fingerprint}"))
        .map_err(|error| CredentialReceiptError::Fingerprint(error.to_string()))
    }

    pub fn verify(&self, binding: &AttemptCredentialBinding) -> Result<(), CredentialReceiptError> {
        if self.candidate_fingerprint != binding.candidate_fingerprint
            || self.credential != binding.credential
            || self.selected_plaintext_holder != binding.selected_plaintext_holder
            || self.claim_epoch != binding.claim_epoch
        {
            return Err(CredentialReceiptError::BindingMismatch);
        }
        if self.actual_realization_kind != binding.selected_realization_kind {
            return Err(CredentialReceiptError::MechanismMismatch);
        }
        if self.receipt_fingerprint != self.expected_fingerprint()? {
            return Err(CredentialReceiptError::FingerprintMismatch);
        }
        Ok(())
    }
}

/// Stable fingerprint used by claim compilation and every execution adapter.
pub fn candidate_fingerprint(
    candidate: &ResolvedModelCandidate,
) -> Result<CandidateFingerprint, CredentialReceiptError> {
    crate::resolution::content_fingerprint(candidate)
        .map(|fingerprint| CandidateFingerprint(format!("sha256:{fingerprint}")))
        .map_err(|error| CredentialReceiptError::Fingerprint(error.to_string()))
}

/// Verify one effect against the unique binding frozen for that candidate.
pub fn verify_credential_realization_receipt(
    bindings: &[AttemptCredentialBinding],
    receipt: &CredentialRealizationReceipt,
) -> Result<(), CredentialReceiptError> {
    let binding = bindings
        .iter()
        .find(|binding| binding.candidate_fingerprint == receipt.candidate_fingerprint)
        .ok_or(CredentialReceiptError::BindingMismatch)?;
    receipt.verify(binding)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialReceiptError {
    #[error("credential realization receipt targets another attempt binding")]
    BindingMismatch,
    #[error("credential realization mechanism differs from the selected mechanism")]
    MechanismMismatch,
    #[error("credential realization receipt fingerprint mismatch")]
    FingerprintMismatch,
    #[error("credential realization fingerprint failed: {0}")]
    Fingerprint(String),
}

/// Infrastructure-neutral sink for a claim-fenced realization receipt.
#[async_trait]
pub trait CredentialRealizationRecorder: Send + Sync {
    async fn record(
        &self,
        receipt: CredentialRealizationReceipt,
    ) -> Result<(), CredentialRealizationRecordError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("credential realization receipt was rejected: {0}")]
pub struct CredentialRealizationRecordError(pub String);

/// Attempt-local projection of durable credential authority. It contains no
/// material and cannot select another candidate, holder, or mechanism.
#[derive(Clone)]
pub struct AttemptCredentialRealization {
    bindings: std::sync::Arc<[AttemptCredentialBinding]>,
    recorder: std::sync::Arc<dyn CredentialRealizationRecorder>,
}

impl AttemptCredentialRealization {
    #[must_use]
    pub fn new(
        bindings: Vec<AttemptCredentialBinding>,
        recorder: std::sync::Arc<dyn CredentialRealizationRecorder>,
    ) -> Self {
        Self {
            bindings: bindings.into(),
            recorder,
        }
    }

    #[must_use]
    pub fn bindings(&self) -> &[AttemptCredentialBinding] {
        &self.bindings
    }

    pub fn binding_for(
        &self,
        candidate: &ResolvedModelCandidate,
    ) -> Result<Option<&AttemptCredentialBinding>, CredentialReceiptError> {
        let fingerprint = candidate_fingerprint(candidate)?;
        Ok(self
            .bindings
            .iter()
            .find(|binding| binding.candidate_fingerprint == fingerprint))
    }

    pub async fn record(
        &self,
        binding: &AttemptCredentialBinding,
    ) -> Result<(), CredentialRealizationRecordError> {
        let receipt = CredentialRealizationReceipt::new(binding, binding.selected_realization_kind)
            .map_err(|error| CredentialRealizationRecordError(error.to_string()))?;
        self.recorder.record(receipt).await
    }
}
