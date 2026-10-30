// @formal surface obligation=worker_credential.host_observation_projection_is_backend_scoped
// @formal reason: These existing secret-free observation values are shared by Control projection and
// @formal reason: Worker discovery. The discovery adapter still validates exact backend identity before
// @formal reason: publishing evidence; moving values here adds no discovery or credential authority.
//! Secret-free host observations consumed by capability publication and diagnostics.

use awaken_credential_contract::{CredentialObservationState, CredentialRef};
use serde::{Deserialize, Deserializer, Serialize};

use crate::AcpCapabilityObservationState;

/// Whether a supported catalog row is launchable on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpDetectionState {
    Detected,
    Missing,
    ProbeFailed,
}

/// Secret-free join input for Worker capability publication and diagnostics.
///
/// Type-ownership rule O1: a host observation carries the existing contract-owned
/// protocol state directly, without conversion or a second enum. This is a
/// compile-time ownership check; the original discovery/projection tests own
/// runtime effects. The backend's module documentation owns negative rule O2.
///
/// ```
/// use awaken_acp_contract::{
///     AcpCapabilityObservationState, AcpDetectionState, AcpHostObservation,
/// };
/// let observation = AcpHostObservation {
///     cli_id: "codex".into(),
///     display_name: "Codex".into(),
///     detection: AcpDetectionState::Detected,
///     version: None,
///     credential_state: None,
///     reason_code: None,
///     capability_state: Some(AcpCapabilityObservationState::Verified),
///     capability_fingerprint: None,
///     capability_reason_code: None,
/// };
/// let _: Option<AcpCapabilityObservationState> = observation.capability_state;
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpHostObservation {
    pub cli_id: String,
    pub display_name: String,
    pub detection: AcpDetectionState,
    pub version: Option<String>,
    pub credential_state: Option<CredentialObservationState>,
    pub reason_code: Option<String>,
    pub capability_state: Option<AcpCapabilityObservationState>,
    pub capability_fingerprint: Option<String>,
    pub capability_reason_code: Option<String>,
}

impl AcpHostObservation {
    #[must_use]
    pub fn detected(&self) -> bool {
        self.detection == AcpDetectionState::Detected
    }
}

/// Secret-free metadata, not a credential value or a login-file location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpLocalCredentialBinding {
    #[serde(deserialize_with = "deserialize_reference")]
    pub credential: CredentialRef,
    pub cli_id: String,
    pub enabled: bool,
}

fn deserialize_reference<'de, D: Deserializer<'de>>(d: D) -> Result<CredentialRef, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictReference {
        id: String,
        revision: u64,
    }
    let value = StrictReference::deserialize(d)?;
    Ok(CredentialRef {
        id: value.id,
        revision: value.revision,
    })
}

impl AcpLocalCredentialBinding {
    pub fn validate(&self, catalog: &[crate::AcpPublicationCapability]) -> Result<(), String> {
        if self.credential.id.trim().is_empty() || self.credential.revision == 0 {
            return Err(
                "WorkerLocal credential requires a nonblank id and positive revision".into(),
            );
        }
        if !catalog
            .iter()
            .any(|row| row.backend_ref == format!("acp:{}", self.cli_id))
        {
            return Err("WorkerLocal binding names an unknown ACP CLI".into());
        }
        Ok(())
    }
}
