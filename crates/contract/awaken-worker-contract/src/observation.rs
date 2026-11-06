// @formal entrypoint worker.acp_capability_is_detected_only_after_a_verified_handshake
// @formal entrypoint worker_credential.dynamic_observation_requires_exact_fact_and_half_open_lease
// @formal entrypoint worker_credential.expired_observation_never_executes
use awaken_acp_contract::{AcpCapabilityObservation, AcpCapabilityObservationState};
use awaken_agent_contract::credential::CredentialRef;
use awaken_credential_contract::CredentialObservationState;
use serde::{Deserialize, Serialize};

use crate::capability_agreement::{CapabilityEvidenceAgreement, merge_capability_evidence};
use crate::registry::{RegisteredWorker, WorkerSnapshot};

/// Closed lease rule for any dynamic Worker observation. Identity, state and
/// coherence are supplied as one exact-match axis by the typed observation;
/// time validity is a half-open interval so old evidence loses authority at
/// `valid_until_ms` and future evidence never becomes prematurely selectable.
#[must_use]
pub(crate) const fn dynamic_observation_admitted(
    exact_verified_fact: bool,
    observed_at_ms: u64,
    now_ms: u64,
    valid_until_ms: u64,
) -> bool {
    exact_verified_fact && observed_at_ms <= now_ms && now_ms < valid_until_ms
}

/// Final admission projection for asynchronous Worker observations.
///
/// Process readiness and static placement are intentionally computed without
/// waiting for Sandbox-backed probes. Missing dynamic evidence can only make a
/// ready Worker ineligible; it can never manufacture readiness or widen static
/// eligibility.
#[must_use]
pub const fn dynamic_evidence_admits(
    process_and_static_eligible: bool,
    credential_evidence_satisfied: bool,
    acp_evidence_satisfied: bool,
) -> bool {
    process_and_static_eligible && credential_evidence_satisfied && acp_evidence_satisfied
}

/// Point-in-time, non-secret credential evidence published by one Worker.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkerCredentialObservation {
    pub credential: CredentialRef,
    pub state: CredentialObservationState,
    pub observed_at_ms: u64,
    /// Exclusive deadline after which this observation is no longer placement
    /// evidence. A missing field from an older sender decodes to zero and
    /// therefore fails closed.
    #[serde(default)]
    pub valid_until_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

impl WorkerCredentialObservation {
    #[must_use]
    pub fn available(credential: CredentialRef, observed_at_ms: u64, valid_until_ms: u64) -> Self {
        Self {
            credential,
            state: CredentialObservationState::Available,
            observed_at_ms,
            valid_until_ms,
            reason_code: None,
        }
    }

    #[must_use]
    pub fn is_selectable_at(&self, credential: &CredentialRef, now_ms: u64) -> bool {
        dynamic_observation_admitted(
            self.state == CredentialObservationState::Available && &self.credential == credential,
            self.observed_at_ms,
            now_ms,
            self.valid_until_ms,
        )
    }
}

/// Worker-leased wrapper around one ACP capability observation. The inner
/// profile is protocol-neutral and secret-free; expiry is Worker authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerAcpCapabilityObservation {
    pub observation: AcpCapabilityObservation,
    #[serde(default)]
    pub valid_until_ms: u64,
}

impl WorkerAcpCapabilityObservation {
    #[must_use]
    pub fn is_selectable_at(
        &self,
        requirement: &WorkerAcpCapabilityRequirement,
        now_ms: u64,
    ) -> bool {
        dynamic_observation_admitted(
            self.observation.state() == AcpCapabilityObservationState::Verified
                && self.observation.backend_ref() == requirement.backend_ref
                && self.observation.fingerprint() == Some(requirement.fingerprint.as_str()),
            self.observation.observed_at_ms(),
            now_ms,
            self.valid_until_ms,
        )
    }
}

/// One coherent ACP handshake that may be pinned into a publication or shown
/// by a management read model. Selection remains in the Worker domain because
/// only this domain owns the liveness, incarnation, credential, and observation
/// fences that make the evidence usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAcpCapabilityEvidence {
    pub adapter_version: String,
    pub fingerprint: String,
    pub negotiated: awaken_acp_contract::NegotiatedAcpCapabilities,
}

/// Exhaustive outcome of selecting live ACP evidence across every Worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifiedAcpCapabilitySelection {
    Available(VerifiedAcpCapabilityEvidence),
    Unavailable,
    Incompatible,
}

/// Select the one coherent live ACP profile across the current Worker view.
/// A credential requirement, when present, must be satisfied by the same
/// Worker snapshot as the capability observation.
#[must_use]
pub fn select_verified_acp_capability<'a>(
    workers: impl IntoIterator<Item = &'a RegisteredWorker>,
    backend_ref: &str,
    credential: Option<&CredentialRef>,
    now_ms: u64,
) -> VerifiedAcpCapabilitySelection {
    let evidence = workers
        .into_iter()
        .map(|registered| &registered.snapshot)
        .filter(|worker| worker_accepts_dynamic_evidence(worker, credential, now_ms))
        .flat_map(|worker| worker.acp_capability_observations.iter())
        .filter(|capability| {
            capability.valid_until_ms > now_ms
                && capability.observation.observed_at_ms() <= now_ms
                && capability.observation.backend_ref() == backend_ref
        })
        .filter_map(|capability| {
            capability.observation.clone().into_verified_evidence().map(
                |(adapter_version, fingerprint, negotiated)| VerifiedAcpCapabilityEvidence {
                    adapter_version,
                    fingerprint,
                    negotiated,
                },
            )
        })
        .collect::<Vec<_>>();
    let mut selected = None;
    let mut agreement = CapabilityEvidenceAgreement::Empty;
    for candidate in evidence {
        let matches_selected = selected.as_ref() == Some(&candidate);
        agreement = merge_capability_evidence(agreement, matches_selected);
        if selected.is_none() {
            selected = Some(candidate);
        }
    }
    match (agreement, selected) {
        (CapabilityEvidenceAgreement::Empty, None) => VerifiedAcpCapabilitySelection::Unavailable,
        (CapabilityEvidenceAgreement::One, Some(only)) => {
            VerifiedAcpCapabilitySelection::Available(only)
        }
        (CapabilityEvidenceAgreement::Conflict, _) => VerifiedAcpCapabilitySelection::Incompatible,
        _ => unreachable!("evidence agreement and selected profile are coherent"),
    }
}

fn worker_accepts_dynamic_evidence(
    worker: &WorkerSnapshot,
    credential: Option<&CredentialRef>,
    now_ms: u64,
) -> bool {
    matches!(worker.standing(now_ms), crate::WorkerStanding::Ready)
        && credential.is_none_or(|credential| {
            worker
                .credential_observations
                .iter()
                .any(|observation| observation.is_selectable_at(credential, now_ms))
        })
}

/// Exact dynamic ACP profile required by an immutable publication.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkerAcpCapabilityRequirement {
    pub backend_ref: String,
    pub fingerprint: String,
}
