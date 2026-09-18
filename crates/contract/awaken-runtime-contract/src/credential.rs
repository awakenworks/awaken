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
        let backend = Backend::from_ref(&candidate.binding().backend_ref);
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
                .acp_backend_realization_kind(&candidate.binding().backend_ref)
                .map_err(AttemptCredentialBindingError::InvalidWorkerCapabilities)?
                .ok_or_else(|| AttemptCredentialBindingError::UnsupportedRealization {
                    boundary: PlaintextBoundary::Workload,
                    backend: candidate.binding().backend_ref.clone(),
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
                    backend: candidate.binding().backend_ref.clone(),
                });
            }
            (Backend::Invalid(_), _) => {
                return Err(AttemptCredentialBindingError::UnsupportedRealization {
                    boundary: holder.boundary,
                    backend: candidate.binding().backend_ref.clone(),
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
