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

/// Remove only the claim-scoped transport carrier. Credential selection,
/// usage, target and plaintext policy remain immutable.
#[must_use]
pub fn credential_access_without_delivery(access: &CredentialAccess) -> CredentialAccess {
    let mut authority = access.clone();
    authority.envelope = None;
    authority
}

/// Compare immutable credential authority while excluding its short-lived
/// claim-scoped transport carrier.
#[must_use]
pub fn credential_access_has_same_authority(
    left: &CredentialAccess,
    right: &CredentialAccess,
) -> bool {
    credential_access_without_delivery(left) == credential_access_without_delivery(right)
}

/// Preserve every publication-time admission check while deferring only the
/// liveness of the envelope that Control will replace after claim.
#[must_use]
fn credential_access_for_claim_admission(access: &CredentialAccess) -> CredentialAccess {
    let mut admitted = access.clone();
    if let Some(envelope) = admitted.envelope.as_mut() {
        match envelope {
            CredentialEnvelope::SealedForWorker {
                expires_at_unix_ms, ..
            }
            | CredentialEnvelope::SealedForWorkload {
                expires_at_unix_ms, ..
            } => *expires_at_unix_ms = u64::MAX,
        }
    }
    admitted
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttemptCredentialBindingError {
    #[error("attempt claim epoch must be greater than zero")]
    InvalidClaimEpoch,
    #[error("credential-bearing inference has no exact plaintext holder request")]
    MissingPlaintextHolder,
    #[error("inference credential usage is incompatible with the selected backend")]
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
        let access = match candidate.provisioning() {
            ModelProvisioning::Provider {
                credential: Some(access),
                ..
            }
            | ModelProvisioning::Remote {
                credential: Some(access),
                ..
            } => access,
            _ => continue,
        };
        let holder = holder.ok_or(AttemptCredentialBindingError::MissingPlaintextHolder)?;
        let backend = candidate.binding().backend();
        match &backend {
            Backend::Native if access.usage != CredentialUsage::ProviderAdapter => {
                return Err(AttemptCredentialBindingError::InvalidCredentialUsage);
            }
            Backend::Acp(_) if !access.usage.is_scalar_process_delivery() => {
                return Err(AttemptCredentialBindingError::InvalidCredentialUsage);
            }
            Backend::Remote(_) if !matches!(access.usage, CredentialUsage::HttpHeader { .. }) => {
                return Err(AttemptCredentialBindingError::InvalidCredentialUsage);
            }
            _ => {}
        }
        let realization = match (&backend, holder.boundary) {
            (Backend::Native, PlaintextBoundary::Worker) => {
                CredentialRealizationKind::WorkerProviderAdapter
            }
            // An ACP backend that advertises no last-mile mechanism is
            // refused. Defaulting it to a process secret environment would
            // hand plaintext to a Workload whose capability evidence never
            // claimed it could receive one.
            (Backend::Acp(_), PlaintextBoundary::Workload) => installed
                .acp_backend_realization_kind(candidate.binding().identity().2)
                .map_err(AttemptCredentialBindingError::InvalidWorkerCapabilities)?
                .ok_or_else(|| AttemptCredentialBindingError::UnsupportedRealization {
                    boundary: PlaintextBoundary::Workload,
                    backend: candidate.binding().identity().2.to_owned(),
                })?,
            (Backend::Acp(_), PlaintextBoundary::Worker) => CredentialRealizationKind::WorkerRelay,
            (Backend::Remote(_), PlaintextBoundary::Worker) => {
                CredentialRealizationKind::WorkerRelay
            }
            (Backend::Native, PlaintextBoundary::Platform) => {
                CredentialRealizationKind::PlatformProviderAdapter
            }
            (Backend::Native | Backend::Acp(_) | Backend::Remote(_), boundary) => {
                return Err(AttemptCredentialBindingError::UnsupportedRealization {
                    boundary,
                    backend: candidate.binding().identity().2.to_owned(),
                });
            }
            (Backend::Invalid(_), _) => {
                return Err(AttemptCredentialBindingError::UnsupportedRealization {
                    boundary: holder.boundary,
                    backend: candidate.binding().identity().2.to_owned(),
                });
            }
        };
        credential_access_for_claim_admission(access).admit(
            holder,
            realization,
            installed,
            now_unix_ms,
        )?;
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

/// Claim-scoped transport for one already-frozen credential authority. Only
/// the envelope may change; a successor claim receives its own epoch delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptCredentialDelivery {
    pub candidate_fingerprint: CandidateFingerprint,
    pub access: CredentialAccess,
    pub claim_epoch: u64,
}

/// Exact request to the deployment-owned delivery authority. It contains no
/// plaintext and cannot select another credential or execution mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptCredentialDeliveryRequest {
    pub candidate_fingerprint: CandidateFingerprint,
    pub published_access: CredentialAccess,
    pub selected_plaintext_holder: PlaintextHolder,
    pub binding: CredentialMaterialBinding,
    pub claim_epoch: u64,
}

#[async_trait]
pub trait AttemptCredentialDeliverySource: Send + Sync {
    async fn deliver(
        &self,
        request: AttemptCredentialDeliveryRequest,
    ) -> Result<AttemptCredentialDelivery, String>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttemptCredentialDeliveryError {
    #[error("credential delivery targets another attempt binding")]
    BindingMismatch,
    #[error("credential delivery changes immutable publication authority")]
    AuthorityMismatch,
    #[error("credential delivery has no recipient envelope")]
    EnvelopeMissing,
    #[error("credential delivery is duplicated for one candidate")]
    DuplicateCandidate,
    #[error("credential delivery candidate fingerprint failed: {0}")]
    Fingerprint(String),
    #[error("credential delivery refresh failed: {0}")]
    Refresh(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptCredentialDeliveryAdmission {
    Exact,
    BindingMismatch,
    AuthorityMismatch,
    EnvelopeMissing,
    DuplicateCandidate,
}

#[must_use]
const fn attempt_credential_delivery_admission(
    duplicate: bool,
    binding_present: bool,
    binding_coordinates_match: bool,
    authority_matches: bool,
    envelope_present: bool,
) -> AttemptCredentialDeliveryAdmission {
    if duplicate {
        AttemptCredentialDeliveryAdmission::DuplicateCandidate
    } else if !binding_present || !binding_coordinates_match {
        AttemptCredentialDeliveryAdmission::BindingMismatch
    } else if !authority_matches {
        AttemptCredentialDeliveryAdmission::AuthorityMismatch
    } else if !envelope_present {
        AttemptCredentialDeliveryAdmission::EnvelopeMissing
    } else {
        AttemptCredentialDeliveryAdmission::Exact
    }
}

fn map_attempt_credential_delivery_admission(
    decision: AttemptCredentialDeliveryAdmission,
) -> Result<(), AttemptCredentialDeliveryError> {
    match decision {
        AttemptCredentialDeliveryAdmission::Exact => Ok(()),
        AttemptCredentialDeliveryAdmission::BindingMismatch => {
            Err(AttemptCredentialDeliveryError::BindingMismatch)
        }
        AttemptCredentialDeliveryAdmission::AuthorityMismatch => {
            Err(AttemptCredentialDeliveryError::AuthorityMismatch)
        }
        AttemptCredentialDeliveryAdmission::EnvelopeMissing => {
            Err(AttemptCredentialDeliveryError::EnvelopeMissing)
        }
        AttemptCredentialDeliveryAdmission::DuplicateCandidate => {
            Err(AttemptCredentialDeliveryError::DuplicateCandidate)
        }
    }
}

/// Verify one claim-scoped delivery against immutable publication authority and
/// the exact binding selected by Dispatch.
pub fn verify_attempt_credential_delivery(
    binding: &AttemptCredentialBinding,
    published_access: &CredentialAccess,
    delivery: &AttemptCredentialDelivery,
) -> Result<(), AttemptCredentialDeliveryError> {
    map_attempt_credential_delivery_admission(attempt_credential_delivery_admission(
        false,
        true,
        delivery.candidate_fingerprint == binding.candidate_fingerprint
            && delivery.access.credential == binding.credential
            && delivery.claim_epoch == binding.claim_epoch,
        credential_access_has_same_authority(&delivery.access, published_access),
        delivery.access.envelope.is_some(),
    ))
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

/// Exact admission result for one materialization receipt against its frozen
/// attempt binding. Persistence adapters consume the public error vocabulary;
/// this closed decision remains the single owner of coordinate precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialRealizationReceiptDecision {
    Exact,
    BindingMismatch,
    MechanismMismatch,
    FingerprintMismatch,
}

/// Durable receipt identity shared by every Dispatch store after binding
/// verification. `Insert` is the only state-changing decision; exact replay is
/// an idempotent stutter and any same-candidate payload change is a collision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialRealizationPersistenceDecision {
    Insert,
    ExactReplay,
    Collision,
}

#[must_use]
pub fn credential_realization_persistence_decision(
    existing: Option<&CredentialRealizationReceipt>,
    proposed: &CredentialRealizationReceipt,
) -> CredentialRealizationPersistenceDecision {
    match existing {
        None => CredentialRealizationPersistenceDecision::Insert,
        Some(existing) if existing == proposed => {
            CredentialRealizationPersistenceDecision::ExactReplay
        }
        Some(_) => CredentialRealizationPersistenceDecision::Collision,
    }
}

#[must_use]
fn credential_realization_receipt_decision(
    binding: &AttemptCredentialBinding,
    receipt: &CredentialRealizationReceipt,
    expected_fingerprint: &str,
) -> CredentialRealizationReceiptDecision {
    if receipt.candidate_fingerprint != binding.candidate_fingerprint
        || receipt.credential != binding.credential
        || receipt.selected_plaintext_holder != binding.selected_plaintext_holder
        || receipt.claim_epoch != binding.claim_epoch
    {
        CredentialRealizationReceiptDecision::BindingMismatch
    } else if receipt.actual_realization_kind != binding.selected_realization_kind {
        CredentialRealizationReceiptDecision::MechanismMismatch
    } else if receipt.receipt_fingerprint != expected_fingerprint {
        CredentialRealizationReceiptDecision::FingerprintMismatch
    } else {
        CredentialRealizationReceiptDecision::Exact
    }
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
        let expected_fingerprint = self.expected_fingerprint()?;
        match credential_realization_receipt_decision(binding, self, &expected_fingerprint) {
            CredentialRealizationReceiptDecision::Exact => Ok(()),
            CredentialRealizationReceiptDecision::BindingMismatch => {
                Err(CredentialReceiptError::BindingMismatch)
            }
            CredentialRealizationReceiptDecision::MechanismMismatch => {
                Err(CredentialReceiptError::MechanismMismatch)
            }
            CredentialRealizationReceiptDecision::FingerprintMismatch => {
                Err(CredentialReceiptError::FingerprintMismatch)
            }
        }
    }
}

#[cfg(kani)]
/// @obligation credential_effect.boundary_preserves_exact_materialization_authority
#[kani::proof]
#[kani::unwind(20)]
fn credential_realization_receipt_requires_every_exact_binding_coordinate() {
    // Causes: C1 candidate fingerprint; C2 credential id; C3 revision;
    // C4 holder boundary; C5 trust domain; C6 realization mechanism;
    // C7 claim epoch; C8 receipt fingerprint. Each cause independently either
    // matches or differs. Effects: E1 all match => Exact; E2 any C1-C5/C7
    // mismatch => BindingMismatch; E3 otherwise C6 mismatch =>
    // MechanismMismatch; E4 otherwise C8 mismatch => FingerprintMismatch.
    // The precedence is the public error contract. SHA-256 implementation and
    // durable claim/store linearization remain outside this coordinate proof.
    let exact = [
        kani::any::<bool>(),
        kani::any::<bool>(),
        kani::any::<bool>(),
        kani::any::<bool>(),
        kani::any::<bool>(),
        kani::any::<bool>(),
        kani::any::<bool>(),
        kani::any::<bool>(),
    ];
    let binding = AttemptCredentialBinding {
        candidate_fingerprint: CandidateFingerprint("candidate-a".into()),
        credential: CredentialRef {
            id: "credential-a".into(),
            revision: 7,
        },
        selected_plaintext_holder: PlaintextHolder::new(PlaintextBoundary::Worker, "holder-a"),
        selected_realization_kind: CredentialRealizationKind::WorkerProviderAdapter,
        claim_epoch: 11,
    };
    let receipt = CredentialRealizationReceipt {
        candidate_fingerprint: CandidateFingerprint(
            if exact[0] {
                "candidate-a"
            } else {
                "candidate-b"
            }
            .into(),
        ),
        credential: CredentialRef {
            id: if exact[1] {
                "credential-a"
            } else {
                "credential-b"
            }
            .into(),
            revision: if exact[2] { 7 } else { 8 },
        },
        selected_plaintext_holder: PlaintextHolder::new(
            if exact[3] {
                PlaintextBoundary::Worker
            } else {
                PlaintextBoundary::Workload
            },
            if exact[4] { "holder-a" } else { "holder-b" },
        ),
        actual_realization_kind: if exact[5] {
            CredentialRealizationKind::WorkerProviderAdapter
        } else {
            CredentialRealizationKind::WorkerRelay
        },
        claim_epoch: if exact[6] { 11 } else { 12 },
        receipt_fingerprint: if exact[7] { "receipt-a" } else { "receipt-b" }.into(),
    };
    let decision = credential_realization_receipt_decision(&binding, &receipt, "receipt-a");
    let binding_exact = exact[0] && exact[1] && exact[2] && exact[3] && exact[4] && exact[6];
    let expected = if !binding_exact {
        CredentialRealizationReceiptDecision::BindingMismatch
    } else if !exact[5] {
        CredentialRealizationReceiptDecision::MechanismMismatch
    } else if !exact[7] {
        CredentialRealizationReceiptDecision::FingerprintMismatch
    } else {
        CredentialRealizationReceiptDecision::Exact
    };
    assert_eq!(decision, expected);
    assert_eq!(
        credential_realization_persistence_decision(None, &receipt),
        CredentialRealizationPersistenceDecision::Insert
    );
    assert_eq!(
        credential_realization_persistence_decision(Some(&receipt), &receipt),
        CredentialRealizationPersistenceDecision::ExactReplay
    );
    let mut collision = receipt.clone();
    collision.claim_epoch = collision.claim_epoch.wrapping_add(1);
    assert_eq!(
        credential_realization_persistence_decision(Some(&receipt), &collision),
        CredentialRealizationPersistenceDecision::Collision
    );
}

/// Stable fingerprint used by claim compilation and every execution adapter.
pub fn candidate_fingerprint(
    candidate: &ResolvedModelCandidate,
) -> Result<CandidateFingerprint, CredentialReceiptError> {
    crate::resolution::content_fingerprint(candidate)
        .map(|fingerprint| CandidateFingerprint(format!("sha256:{fingerprint}")))
        .map_err(|error| CredentialReceiptError::Fingerprint(error.to_string()))
}

/// Derive the one authenticated material binding used by publication delivery,
/// claim redelivery and last-mile resolution. Keeping this with the candidate
/// contract prevents those three boundaries from hashing subtly different
/// provider or remote coordinates.
pub fn candidate_credential_material_binding(
    candidate: &ResolvedModelCandidate,
) -> Option<(&CredentialAccess, CredentialMaterialBinding)> {
    match candidate.provisioning() {
        ModelProvisioning::Provider {
            provider_ref,
            scope_id,
            credential: Some(access),
            endpoint,
            ..
        } => Some((
            access,
            CredentialMaterialBinding::for_target(
                scope_id.as_str(),
                &(provider_ref, endpoint),
                &access.usage,
            ),
        )),
        ModelProvisioning::Remote {
            scope_id,
            credential: Some(access),
            security_fingerprint,
        } => Some((
            access,
            CredentialMaterialBinding::for_target(
                scope_id.as_str(),
                &(&candidate.binding().identity().2, security_fingerprint),
                &access.usage,
            ),
        )),
        _ => None,
    }
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
    deliveries: std::sync::Arc<[AttemptCredentialDelivery]>,
    refresh: Option<std::sync::Arc<dyn AttemptCredentialDeliverySource>>,
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
            deliveries: std::sync::Arc::from([]),
            refresh: None,
            recorder,
        }
    }

    pub fn new_with_deliveries(
        bindings: Vec<AttemptCredentialBinding>,
        deliveries: Vec<AttemptCredentialDelivery>,
        recorder: std::sync::Arc<dyn CredentialRealizationRecorder>,
    ) -> Result<Self, AttemptCredentialDeliveryError> {
        let mut seen = std::collections::BTreeSet::new();
        for delivery in &deliveries {
            let duplicate = !seen.insert(delivery.candidate_fingerprint.clone());
            let binding = bindings
                .iter()
                .find(|binding| binding.candidate_fingerprint == delivery.candidate_fingerprint);
            let coordinates_match = binding.is_some_and(|binding| {
                binding.credential == delivery.access.credential
                    && binding.claim_epoch == delivery.claim_epoch
            });
            map_attempt_credential_delivery_admission(attempt_credential_delivery_admission(
                duplicate,
                binding.is_some(),
                coordinates_match,
                true,
                delivery.access.envelope.is_some(),
            ))?;
        }
        Ok(Self {
            bindings: bindings.into(),
            deliveries: deliveries.into(),
            refresh: None,
            recorder,
        })
    }

    #[must_use]
    pub fn with_delivery_refresh(
        mut self,
        refresh: std::sync::Arc<dyn AttemptCredentialDeliverySource>,
    ) -> Self {
        self.refresh = Some(refresh);
        self
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

    /// Select the exact ephemeral delivery and prove it changes no publication
    /// authority before any plaintext resolver sees it.
    pub async fn delivered_access_for(
        &self,
        candidate: &ResolvedModelCandidate,
        published_access: &CredentialAccess,
    ) -> Result<Option<CredentialAccess>, AttemptCredentialDeliveryError> {
        let fingerprint = candidate_fingerprint(candidate)
            .map_err(|error| AttemptCredentialDeliveryError::Fingerprint(error.to_string()))?;
        let binding = self
            .bindings
            .iter()
            .find(|binding| binding.candidate_fingerprint == fingerprint)
            .ok_or(AttemptCredentialDeliveryError::BindingMismatch)?;
        if let Some(refresh) = &self.refresh {
            let Some((candidate_access, material_binding)) =
                candidate_credential_material_binding(candidate)
            else {
                return Err(AttemptCredentialDeliveryError::BindingMismatch);
            };
            if !credential_access_has_same_authority(candidate_access, published_access)
                || binding.credential != candidate_access.credential
            {
                return Err(AttemptCredentialDeliveryError::BindingMismatch);
            }
            let delivery = refresh
                .deliver(AttemptCredentialDeliveryRequest {
                    candidate_fingerprint: fingerprint,
                    published_access: published_access.clone(),
                    selected_plaintext_holder: binding.selected_plaintext_holder.clone(),
                    binding: material_binding,
                    claim_epoch: binding.claim_epoch,
                })
                .await
                .map_err(AttemptCredentialDeliveryError::Refresh)?;
            verify_attempt_credential_delivery(binding, published_access, &delivery)?;
            return Ok(Some(delivery.access));
        }
        let Some(delivery) = self
            .deliveries
            .iter()
            .find(|delivery| delivery.candidate_fingerprint == fingerprint)
        else {
            return Ok(None);
        };
        verify_attempt_credential_delivery(binding, published_access, delivery)?;
        Ok(Some(delivery.access.clone()))
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

#[cfg(kani)]
/// @obligation credential.attempt_delivery_is_exact_epoch_bound_and_authority_preserving
#[kani::proof]
fn attempt_delivery_requires_unique_exact_binding_envelope_and_unchanged_authority() {
    // Cause/effect proof input: symbolic candidate, binding coordinates,
    // publication authority and envelope are converted by the same comparisons
    // and presence checks used by production. Exact admission is equivalent to
    // all five derived causes holding.
    let seen_candidate: Option<u8> = kani::any();
    let delivery_candidate: u8 = kani::any();
    let binding: Option<(u8, u8, u8)> = kani::any();
    let delivery_coordinates: (u8, u8, u8) = kani::any();
    let published_authority: u8 = kani::any();
    let delivered_authority: u8 = kani::any();
    let envelope: Option<u8> = kani::any();
    let decision = attempt_credential_delivery_admission(
        seen_candidate == Some(delivery_candidate),
        binding.is_some(),
        binding.is_some_and(|coordinates| coordinates == delivery_coordinates),
        published_authority == delivered_authority,
        envelope.is_some(),
    );
    assert_eq!(
        decision == AttemptCredentialDeliveryAdmission::Exact,
        seen_candidate != Some(delivery_candidate)
            && binding.is_some()
            && binding.is_some_and(|coordinates| coordinates == delivery_coordinates)
            && published_authority == delivered_authority
            && envelope.is_some()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolved::{BackendModelSelection, ModelBinding, ResolvedModelCandidate};

    /// The one door through which this module's fixtures build a model binding.
    ///
    /// `ModelBinding::new` asserts coordinates instead of admitting them, and a
    /// file that calls it once per fixture has that many places where a coordinate
    /// can be written without the admission in view. Collapsing them to one named
    /// door is the migration the producer ratchet counts; what is left here is a
    /// single call site, not thirteen.
    #[cfg(test)]
    fn fixture_binding(
        provider_identity_ref: &str,
        model_ref: &str,
        backend_ref: &str,
    ) -> ModelBinding {
        ModelBinding::new(provider_identity_ref, model_ref, backend_ref)
    }

    #[test]
    fn backend_owned_candidates_never_compile_material_authority() {
        // Cause graph: BackendOwned -> exact Worker-local liveness fence -> ACP
        // process. It must bypass the provider-material compiler entirely.
        //
        // Decision table: Default and Exact model policies both produce zero
        // material bindings, even when no plaintext holder is available.
        for selection in [BackendModelSelection::Default, BackendModelSelection::Exact] {
            let candidate = ResolvedModelCandidate::try_backend_owned(
                fixture_binding(
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
                "test",
                "sha256:test-capability",
                Default::default(),
            )
            .expect("coherent backend-owned candidate");
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

    #[tokio::test]
    async fn attempt_delivery_is_exact_epoch_bound_and_authority_preserving() {
        // Delivery decision table: C1 candidate/binding/epoch exact; C2 access
        // changes only envelope; C3 duplicate, foreign epoch or changed policy.
        // C4 a claim-bound refresh returns another exact carrier; C5 it fails or
        // changes the epoch. R1 C1+C2 => select fresh access. R2 C3(any) =>
        // reject construction or lookup. R3 C1+C4 => use the refreshed carrier;
        // R4 C5 => fail before materialization. This keeps ephemeral transport
        // outside publication identity without allowing it to widen authority.
        struct Recorder;
        #[async_trait]
        impl CredentialRealizationRecorder for Recorder {
            async fn record(
                &self,
                _: CredentialRealizationReceipt,
            ) -> Result<(), CredentialRealizationRecordError> {
                Ok(())
            }
        }
        struct Refresh(Result<AttemptCredentialDelivery, String>);
        #[async_trait]
        impl AttemptCredentialDeliverySource for Refresh {
            async fn deliver(
                &self,
                _: AttemptCredentialDeliveryRequest,
            ) -> Result<AttemptCredentialDelivery, String> {
                self.0.clone()
            }
        }
        let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker");
        let published = CredentialAccess::new(
            CredentialRef {
                id: "credential-a".into(),
                revision: 3,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::ProviderAdapter,
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        )
        .with_target(CredentialTarget::new(
            CredentialPurpose::ProviderAdapter,
            "deepseek",
        ))
        .with_envelope(CredentialEnvelope::SealedForWorker {
            envelope_ref: SealedCredentialEnvelopeRef {
                id: "old".into(),
                payload_fingerprint: "old".into(),
            },
            recipient: holder.trust_domain.clone(),
            expires_at_unix_ms: 1,
        });
        let candidate = ResolvedModelCandidate::try_provider(
            fixture_binding("deepseek", "deepseek-v4-pro", "genai"),
            "deepseek@1",
            "deepseek.open_ai_chat@1",
            "workspace-a",
            Some(published.clone()),
            crate::InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_chat".into(),
                base_url: "https://api.deepseek.com".into(),
                upstream_model: "deepseek-v4-pro".into(),
                processing_placement: None,
            },
        )
        .unwrap();
        let capabilities = CredentialRealizationCapabilities {
            holders: [holder.clone()].into_iter().collect(),
            material_sources: [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect(),
            realization_kinds: [CredentialRealizationKind::WorkerProviderAdapter]
                .into_iter()
                .collect(),
            recipient_bound_envelopes: true,
            envelope_required: true,
            ..Default::default()
        };
        let bindings = compile_candidate_credential_bindings(
            &[&candidate],
            Some(&holder),
            &capabilities,
            7,
            10,
        )
        .expect("claim identity accepts expired publication delivery");
        let fresh = credential_access_without_delivery(&published).with_envelope(
            CredentialEnvelope::SealedForWorker {
                envelope_ref: SealedCredentialEnvelopeRef {
                    id: "fresh".into(),
                    payload_fingerprint: "fresh".into(),
                },
                recipient: holder.trust_domain.clone(),
                expires_at_unix_ms: 20,
            },
        );
        let delivery = AttemptCredentialDelivery {
            candidate_fingerprint: bindings[0].candidate_fingerprint.clone(),
            access: fresh.clone(),
            claim_epoch: 7,
        };
        let realization = AttemptCredentialRealization::new_with_deliveries(
            bindings.clone(),
            vec![delivery.clone()],
            std::sync::Arc::new(Recorder),
        )
        .expect("R1");
        assert_eq!(
            realization
                .delivered_access_for(&candidate, &published)
                .await
                .expect("R1")
                .unwrap(),
            fresh
        );
        let mut newer = delivery.clone();
        newer.access.envelope = Some(CredentialEnvelope::SealedForWorker {
            envelope_ref: SealedCredentialEnvelopeRef {
                id: "newer".into(),
                payload_fingerprint: "newer".into(),
            },
            recipient: holder.trust_domain.clone(),
            expires_at_unix_ms: 30,
        });
        let refreshed = AttemptCredentialRealization::new_with_deliveries(
            bindings.clone(),
            vec![delivery.clone()],
            std::sync::Arc::new(Recorder),
        )
        .expect("R3")
        .with_delivery_refresh(std::sync::Arc::new(Refresh(Ok(newer.clone()))));
        assert_eq!(
            refreshed
                .delivered_access_for(&candidate, &published)
                .await
                .expect("R3"),
            Some(newer.access.clone()),
            "R3 refresh replaces only the carrier"
        );
        let failed = AttemptCredentialRealization::new_with_deliveries(
            bindings.clone(),
            vec![delivery.clone()],
            std::sync::Arc::new(Recorder),
        )
        .expect("R4")
        .with_delivery_refresh(std::sync::Arc::new(Refresh(Err("unavailable".into()))));
        assert!(matches!(
            failed.delivered_access_for(&candidate, &published).await,
            Err(AttemptCredentialDeliveryError::Refresh(_))
        ));
        newer.claim_epoch += 1;
        let stale = AttemptCredentialRealization::new_with_deliveries(
            bindings.clone(),
            vec![delivery.clone()],
            std::sync::Arc::new(Recorder),
        )
        .expect("R4")
        .with_delivery_refresh(std::sync::Arc::new(Refresh(Ok(newer))));
        assert_eq!(
            stale.delivered_access_for(&candidate, &published).await,
            Err(AttemptCredentialDeliveryError::BindingMismatch),
            "R4 changed epoch is rejected"
        );
        for invalid in [
            AttemptCredentialDelivery {
                claim_epoch: 8,
                ..delivery.clone()
            },
            AttemptCredentialDelivery {
                access: {
                    let mut changed = fresh.clone();
                    changed.policy = CredentialExecutionPolicy::exact(
                        PlaintextHolder::new(PlaintextBoundary::Worker, "foreign"),
                        ModelExposurePolicy::Forbidden,
                    );
                    changed
                },
                ..delivery.clone()
            },
        ] {
            let result = AttemptCredentialRealization::new_with_deliveries(
                bindings.clone(),
                vec![invalid],
                std::sync::Arc::new(Recorder),
            );
            if let Ok(realization) = result {
                assert!(
                    realization
                        .delivered_access_for(&candidate, &published)
                        .await
                        .is_err(),
                    "R2 changed authority rejects at lookup"
                );
            }
        }
        assert!(matches!(
            AttemptCredentialRealization::new_with_deliveries(
                bindings,
                vec![delivery.clone(), delivery],
                std::sync::Arc::new(Recorder),
            ),
            Err(AttemptCredentialDeliveryError::DuplicateCandidate)
        ));
    }

    #[test]
    fn routed_acp_accepts_a_published_process_environment_usage_only_for_acp() {
        let holder =
            PlaintextHolder::new(PlaintextBoundary::Workload, SELF_HOSTED_ACP_TRUST_DOMAIN);
        let access = CredentialAccess::new(
            CredentialRef {
                id: "claude-setup".into(),
                revision: 1,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::process_env("CLAUDE_CODE_OAUTH_TOKEN"),
            CredentialExecutionPolicy::self_hosted_provider(),
        );
        let candidate = ResolvedModelCandidate::try_provider_with_acp(
            fixture_binding("anthropic", "claude-test", "acp:claude"),
            "anthropic@1",
            "anthropic-messages@1",
            "workspace-a",
            Some(access.clone()),
            crate::InferenceEndpoint {
                adapter_kind: "anthropic".into(),
                api_dialect: "anthropic_messages".into(),
                base_url: "https://api.anthropic.com/v1".into(),
                upstream_model: "claude-test".into(),
                processing_placement: None,
            },
            crate::resolved::AcpExecutionProfile {
                supports_tool_aliases: false,
                capability_fingerprint: "sha256:claude-capability".into(),
                capability_adapter_version: "test".into(),
                session_configuration: Default::default(),
            },
        )
        .expect("coherent ACP provider candidate");
        // The installed profile must advertise the exact last-mile mechanism
        // of this ACP backend. Nothing infers it from the realization kind
        // alone: an adapter that claims nothing receives nothing.
        let capabilities = CredentialRealizationCapabilities {
            holders: [holder.clone()].into_iter().collect(),
            material_sources: [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect(),
            realization_kinds: [CredentialRealizationKind::ProcessSecretEnvironment]
                .into_iter()
                .collect(),
            extension_consumers: [(
                format!("{ACP_CREDENTIAL_CONSUMER_PREFIX}acp:claude"),
                [PROCESS_SECRET_ENVIRONMENT_MATERIAL_TYPE.to_string()]
                    .into_iter()
                    .collect(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert_eq!(
            compile_candidate_credential_bindings(
                &[&candidate],
                Some(&holder),
                &capabilities,
                1,
                0,
            )
            .expect("ACP environment usage")
            .len(),
            1
        );

        let native = ResolvedModelCandidate::try_provider(
            fixture_binding("anthropic", "claude-test", "genai"),
            "anthropic@1",
            "anthropic-messages@1",
            "workspace-a",
            Some(access),
            crate::InferenceEndpoint {
                adapter_kind: "anthropic".into(),
                api_dialect: "anthropic_messages".into(),
                base_url: "https://api.anthropic.com/v1".into(),
                upstream_model: "claude-test".into(),
                processing_placement: None,
            },
        )
        .expect("coherent native provider candidate");
        assert_eq!(
            compile_candidate_credential_bindings(&[&native], Some(&holder), &capabilities, 1, 0,),
            Err(AttemptCredentialBindingError::InvalidCredentialUsage)
        );
    }

    /// An ACP backend that advertises no last-mile mechanism must be refused.
    /// The retired behaviour silently defaulted it to a process secret
    /// environment, handing plaintext to a Workload on evidence that never
    /// claimed it could receive any.
    #[test]
    fn acp_backend_without_advertised_delivery_is_refused_not_defaulted() {
        let holder =
            PlaintextHolder::new(PlaintextBoundary::Workload, SELF_HOSTED_ACP_TRUST_DOMAIN);
        let access = CredentialAccess::new(
            CredentialRef {
                id: "claude-setup".into(),
                revision: 1,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::process_env("CLAUDE_CODE_OAUTH_TOKEN"),
            CredentialExecutionPolicy::self_hosted_provider(),
        );
        let candidate = ResolvedModelCandidate::try_provider_with_acp(
            fixture_binding("anthropic", "claude-test", "acp:claude"),
            "anthropic@1",
            "anthropic-messages@1",
            "workspace-a",
            Some(access),
            crate::InferenceEndpoint {
                adapter_kind: "anthropic".into(),
                api_dialect: "anthropic_messages".into(),
                base_url: "https://api.anthropic.com/v1".into(),
                upstream_model: "claude-test".into(),
                processing_placement: None,
            },
            crate::resolved::AcpExecutionProfile {
                supports_tool_aliases: false,
                capability_fingerprint: "sha256:claude-capability".into(),
                capability_adapter_version: "test".into(),
                session_configuration: Default::default(),
            },
        )
        .expect("coherent ACP provider candidate");
        // Holder, material source and realization kind all match; only the
        // per-backend mechanism declaration is absent.
        let silent = CredentialRealizationCapabilities {
            holders: [holder.clone()].into_iter().collect(),
            material_sources: [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect(),
            realization_kinds: [CredentialRealizationKind::ProcessSecretEnvironment]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            compile_candidate_credential_bindings(&[&candidate], Some(&holder), &silent, 1, 0),
            Err(AttemptCredentialBindingError::UnsupportedRealization {
                boundary: PlaintextBoundary::Workload,
                backend: "acp:claude".into(),
            })
        );
    }

    #[test]
    fn routed_acp_claim_uses_the_selected_backends_advertised_delivery_kind() {
        // Independent ACP profiles coexist on one Worker. The claim compiler
        // must retain backend-to-mechanism correlation instead of selecting a
        // kind from the aggregate union.
        let holder =
            PlaintextHolder::new(PlaintextBoundary::Workload, SELF_HOSTED_ACP_TRUST_DOMAIN);
        let profile = |backend_ref: &str, kind: CredentialRealizationKind, material_type: &str| {
            CredentialRealizationCapabilities {
                holders: [holder.clone()].into_iter().collect(),
                material_sources: [CredentialMaterialSource::ControlPlaneReference]
                    .into_iter()
                    .collect(),
                realization_kinds: [kind].into_iter().collect(),
                extension_consumers: [(
                    format!("{ACP_CREDENTIAL_CONSUMER_PREFIX}{backend_ref}"),
                    [material_type.to_string()].into_iter().collect(),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            }
        };
        let capabilities = CredentialRealizationCapabilities::alternatives([
            profile(
                "acp:environment-adapter",
                CredentialRealizationKind::ProcessSecretEnvironment,
                PROCESS_SECRET_ENVIRONMENT_MATERIAL_TYPE,
            ),
            profile(
                "acp:file-adapter",
                CredentialRealizationKind::PrivateSecretFile,
                PRIVATE_SECRET_FILE_MATERIAL_TYPE,
            ),
        ]);
        let candidate = |backend_ref: &str| {
            ResolvedModelCandidate::try_provider_with_acp(
                fixture_binding("provider", "model", backend_ref),
                "provider@1",
                "route@1",
                "workspace-a",
                Some(
                    CredentialAccess::new(
                        CredentialRef {
                            id: "provider-key".into(),
                            revision: 1,
                        },
                        CredentialMaterialSource::ControlPlaneReference,
                        CredentialUsage::ProviderAdapter,
                        CredentialExecutionPolicy::self_hosted_provider(),
                    )
                    .with_target(CredentialTarget::new(
                        CredentialPurpose::ProviderAdapter,
                        "provider",
                    )),
                ),
                crate::InferenceEndpoint {
                    adapter_kind: "generic".into(),
                    api_dialect: "generic".into(),
                    base_url: "https://provider.invalid".into(),
                    upstream_model: "model".into(),
                    processing_placement: None,
                },
                crate::resolved::AcpExecutionProfile {
                    supports_tool_aliases: false,
                    capability_fingerprint: "sha256:test-capability".into(),
                    capability_adapter_version: "test".into(),
                    session_configuration: Default::default(),
                },
            )
            .expect("coherent ACP provider candidate")
        };

        for (backend_ref, expected) in [
            (
                "acp:environment-adapter",
                CredentialRealizationKind::ProcessSecretEnvironment,
            ),
            (
                "acp:file-adapter",
                CredentialRealizationKind::PrivateSecretFile,
            ),
        ] {
            let candidate = candidate(backend_ref);
            let bindings = compile_candidate_credential_bindings(
                &[&candidate],
                Some(&holder),
                &capabilities,
                7,
                0,
            )
            .expect("backend-correlated admission");
            assert_eq!(bindings[0].selected_realization_kind, expected);
        }
    }

    #[test]
    fn remote_candidates_compile_only_claim_fenced_http_header_authority() {
        // Cause graph:
        // C1 Remote publication has an exact credential and verified-origin
        // target; C2 usage is an HTTP header; C3 the selected holder is the
        // Worker; C4 WorkerRelay is installed. Effects: E1 compile one exact
        // attempt binding; E2 an
        // anonymous Remote compiles no authority; E3 a non-header usage fails
        // before materialization.
        //
        // Decision table:
        // | Rule | C1 | C2 | C3+C4 | Effect |
        // | R1   | Y  | Y  | Y     | E1     |
        // | R2   | N  | -  | -     | E2     |
        // | R3   | Y  | N  | Y     | E3     |
        let holder =
            PlaintextHolder::new(PlaintextBoundary::Worker, SELF_HOSTED_WORKER_TRUST_DOMAIN);
        let capabilities = CredentialRealizationCapabilities {
            holders: [holder.clone()].into_iter().collect(),
            material_sources: [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect(),
            realization_kinds: [CredentialRealizationKind::WorkerRelay]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let remote = |credential| {
            ResolvedModelCandidate::try_remote(
                fixture_binding("", "", "a2a:https://agent.example"),
                "workspace-a",
                credential,
                "sha256:card-security",
            )
            .expect("coherent remote candidate")
        };
        let access = |usage| {
            CredentialAccess::new(
                CredentialRef {
                    id: "remote-key".into(),
                    revision: 4,
                },
                CredentialMaterialSource::ControlPlaneReference,
                usage,
                CredentialExecutionPolicy::self_hosted_provider(),
            )
            .with_target(CredentialTarget::new(
                CredentialPurpose::RemoteAgentAuthorization,
                "https://agent.example",
            ))
        };

        let authenticated = remote(Some(access(CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        })));
        let bindings = compile_candidate_credential_bindings(
            &[&authenticated],
            Some(&holder),
            &capabilities,
            7,
            0,
        )
        .expect("remote HTTP-header authority");
        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings[0].selected_realization_kind,
            CredentialRealizationKind::WorkerRelay
        );

        assert!(
            compile_candidate_credential_bindings(
                &[&remote(None)],
                None,
                &CredentialRealizationCapabilities::default(),
                7,
                0,
            )
            .expect("anonymous remote")
            .is_empty()
        );

        let invalid = remote(Some(access(CredentialUsage::ProviderAdapter)));
        assert_eq!(
            compile_candidate_credential_bindings(&[&invalid], Some(&holder), &capabilities, 7, 0,),
            Err(AttemptCredentialBindingError::InvalidCredentialUsage)
        );
    }
}
