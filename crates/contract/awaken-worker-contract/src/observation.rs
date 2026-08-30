use awaken_acp_contract::{AcpCapabilityObservation, AcpCapabilityObservationState};
use awaken_credential_contract::{
    CredentialObservationState as WorkerCredentialState, CredentialRef as WorkerCredentialRevision,
};
use serde::{Deserialize, Serialize};

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
    pub credential: WorkerCredentialRevision,
    pub state: WorkerCredentialState,
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
    pub fn available(
        credential: WorkerCredentialRevision,
        observed_at_ms: u64,
        valid_until_ms: u64,
    ) -> Self {
        Self {
            credential,
            state: WorkerCredentialState::Available,
            observed_at_ms,
            valid_until_ms,
            reason_code: None,
        }
    }

    #[must_use]
    pub fn is_selectable_at(&self, credential: &WorkerCredentialRevision, now_ms: u64) -> bool {
        dynamic_observation_admitted(
            self.state == WorkerCredentialState::Available && &self.credential == credential,
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

/// Exact dynamic ACP profile required by an immutable publication.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkerAcpCapabilityRequirement {
    pub backend_ref: String,
    pub fingerprint: String,
}
