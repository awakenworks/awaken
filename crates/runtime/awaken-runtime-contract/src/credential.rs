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

use crate::resolved::{Backend, ModelProvisioning, ResolvedModelCandidate};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttemptCredentialBindingError {
    #[error("attempt claim epoch must be greater than zero")]
    InvalidClaimEpoch,
    #[error("credential-bearing inference has no exact plaintext holder request")]
    MissingPlaintextHolder,
    #[error("inference credential usage must be provider_adapter")]
    InvalidCredentialUsage,
    #[error("plaintext boundary {boundary:?} is unsupported for inference backend {backend}")]
    UnsupportedRealization {
        boundary: PlaintextBoundary,
        backend: String,
    },
    #[error("published model candidate fingerprint failed: {0}")]
    Fingerprint(String),
    #[error("published model candidate is duplicated in the selected fallback set")]
    DuplicateCandidate,
    #[error("invalid Worker credential capability evidence: {0}")]
    InvalidWorkerCapabilities(String),
    #[error(transparent)]
    Admission(#[from] CredentialAdmissionError),
}

/// Compile one selected inference candidate set into exact attempt authority.
/// Durable dispatch and process-local direct execution share this pure compiler;
/// only their ownership/receipt persistence adapters differ.
pub fn compile_candidate_credential_bindings(
    candidates: &[&ResolvedModelCandidate],
    holder: Option<&PlaintextHolder>,
    installed: &CredentialRealizationCapabilities,
    claim_epoch: u64,
    now_unix_ms: u64,
) -> Result<Vec<AttemptCredentialBinding>, AttemptCredentialBindingError> {
    if claim_epoch == 0 {
        return Err(AttemptCredentialBindingError::InvalidClaimEpoch);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut bindings = Vec::new();
    for candidate in candidates {
        let ModelProvisioning::Provider {
            credential: Some(access),
            ..
        } = &candidate.provisioning
        else {
            continue;
        };
        if access.usage != CredentialUsage::ProviderAdapter {
            return Err(AttemptCredentialBindingError::InvalidCredentialUsage);
        }
        let holder = holder.ok_or(AttemptCredentialBindingError::MissingPlaintextHolder)?;
        let backend = Backend::from_ref(&candidate.binding.backend_ref);
        let realization = match (&backend, holder.boundary) {
            (Backend::Native, PlaintextBoundary::Worker) => {
                CredentialRealizationKind::WorkerProviderAdapter
            }
            (Backend::Acp { .. }, PlaintextBoundary::Workload) => {
                CredentialRealizationKind::ProcessSecretEnvironment
            }
            (Backend::Acp { .. }, PlaintextBoundary::Worker) => {
                CredentialRealizationKind::WorkerRelay
            }
            (Backend::Native, PlaintextBoundary::Platform) => {
                CredentialRealizationKind::PlatformProviderAdapter
            }
            (Backend::Native | Backend::Acp { .. } | Backend::Remote { .. }, boundary) => {
                return Err(AttemptCredentialBindingError::UnsupportedRealization {
                    boundary,
                    backend: candidate.binding.backend_ref.clone(),
                });
            }
        };
        access.admit(holder, realization, installed, now_unix_ms)?;
        let fingerprint = candidate_fingerprint(candidate)
            .map_err(|error| AttemptCredentialBindingError::Fingerprint(error.to_string()))?;
        if !seen.insert(fingerprint.clone()) {
            return Err(AttemptCredentialBindingError::DuplicateCandidate);
        }
        bindings.push(AttemptCredentialBinding {
            candidate_fingerprint: fingerprint,
            credential: access.credential.clone(),
            selected_plaintext_holder: holder.clone(),
            selected_realization_kind: realization,
            claim_epoch,
        });
    }
    Ok(bindings)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolved::{BackendModelSelection, ModelBinding, ResolvedModelCandidate};

    #[test]
    fn backend_owned_candidates_never_compile_material_authority() {
        // Cause graph: BackendOwned -> exact Worker-local liveness fence -> ACP
        // process. It must bypass the provider-material compiler entirely.
        //
        // Decision table: Default and Exact model policies both produce zero
        // material bindings, even when no plaintext holder is available.
        for selection in [BackendModelSelection::Default, BackendModelSelection::Exact] {
            let candidate = ResolvedModelCandidate::backend_owned(
                ModelBinding::new(
                    "local-codex",
                    if selection == BackendModelSelection::Exact {
                        "gpt-exact"
                    } else {
                        ""
                    },
                    "acp:codex",
                ),
                CredentialRef {
                    id: "local-codex".into(),
                    revision: 3,
                },
                selection,
            );
            assert_eq!(
                compile_candidate_credential_bindings(
                    &[&candidate],
                    None,
                    &CredentialRealizationCapabilities::default(),
                    1,
                    0,
                )
                .unwrap(),
                Vec::new()
            );
        }
    }
}
