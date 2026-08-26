//! Exact publication into an external credential custody boundary.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;
use serde::{Deserialize, Serialize};

use crate::{
    CredentialAccess, CredentialEnvelopeIssuer, CredentialMaterialError, CredentialUsage,
    PlaintextHolder,
};

/// Exact non-secret execution binding supplied to the material-source adapter.
/// The fingerprint is computed from the authoritative consumer target together
/// with its usage; adapters compare it with recipient-bound sealed claims and
/// never infer or enumerate a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialMaterialBinding {
    pub workspace_id: String,
    pub target_use_fingerprint: String,
}

impl CredentialMaterialBinding {
    #[must_use]
    pub fn for_target<T: Serialize>(
        workspace_id: impl Into<String>,
        target: &T,
        usage: &CredentialUsage,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            target_use_fingerprint: awaken_agent_contract::stable_fingerprint(&(target, usage)),
        }
    }

    pub fn validate(&self) -> Result<(), CredentialMaterialError> {
        if self.workspace_id.trim().is_empty() || self.target_use_fingerprint.trim().is_empty() {
            return Err(CredentialMaterialError::BindingMismatch);
        }
        Ok(())
    }
}

/// The one plaintext delivery mechanism selected by a product composition.
///
/// Envelope issuance and external custody are alternatives, not independent
/// observers. Encoding that choice as an enum prevents one resolved secret
/// from being released across two infrastructure boundaries.
#[derive(Clone)]
pub enum CredentialMaterialDelivery {
    RecipientEnvelope(Arc<dyn CredentialEnvelopeIssuer>),
    ExternalCustody(Arc<dyn CredentialMaterialCustodian>),
}

/// One exact, already-authorized publication into an external plaintext
/// custody boundary. The caller remains the credential authority and supplies
/// the immutable revision and binding; the implementation must be idempotent
/// for that coordinate and must not retain the plaintext in process memory.
pub struct CredentialCustodyPublication {
    pub access: CredentialAccess,
    pub selected_holder: PlaintextHolder,
    pub binding: CredentialMaterialBinding,
    pub material: RedactedString,
}

#[async_trait]
pub trait CredentialMaterialCustodian: Send + Sync {
    /// Returns whether this custody boundary owns the selected plaintext path.
    /// A custodian must not observe material for paths it does not own.
    fn handles(&self, selected_holder: &PlaintextHolder, usage: &crate::CredentialUsage) -> bool;

    async fn publish(&self, publication: CredentialCustodyPublication) -> Result<(), String>;
}

/// Stable authenticated-data identity for one recipient-bound credential
/// payload. Both an issuer and the exact material resolver call this helper;
/// deployments must not invent another fingerprint over the same authority.
#[must_use]
pub fn credential_envelope_payload_fingerprint(
    access: &CredentialAccess,
    selected_holder: &PlaintextHolder,
    binding: &CredentialMaterialBinding,
) -> String {
    if let Some(target) = &access.target {
        awaken_agent_contract::stable_fingerprint(&(
            &access.credential,
            access.material_source,
            target,
            &access.usage,
            &access.policy,
            selected_holder,
            binding,
        ))
    } else {
        // Cause: an envelope was issued through the legacy, undescribed path.
        // Effect: preserve its authenticated-data identity byte-for-byte while
        // described envelopes additionally bind the exact target contract.
        awaken_agent_contract::stable_fingerprint(&(
            &access.credential,
            access.material_source,
            &access.usage,
            &access.policy,
            selected_holder,
            binding,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CredentialExecutionPolicy, CredentialMaterialSource, CredentialPurpose, CredentialRef,
        ModelExposurePolicy, PlaintextBoundary,
    };

    /// Envelope identity cause/effect graph: C1 legacy access omits target; C2
    /// described access has one target; C3 only target changes. E1 legacy wire
    /// and fingerprint remain byte-compatible; E2 described wire contains one
    /// target and one usage truth; E3 target drift changes the authenticated
    /// payload fingerprint.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// |---|---|---|---|---|
    /// | F1 | T | F | F | E1 |
    /// | F2 | F | T | F | E2 |
    /// | F3 | F | T | T | E3 |
    #[test]
    fn target_extension_preserves_legacy_and_binds_described_envelopes_once() {
        let holder =
            PlaintextHolder::new(PlaintextBoundary::Worker, "spiffe://example.test/worker");
        let binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &("repository-a", 1_u64),
            &CredentialUsage::HttpBasicAuth,
        );
        let legacy = CredentialAccess::new(
            CredentialRef {
                id: "credential-a".into(),
                revision: 1,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpBasicAuth,
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        );
        let legacy_expected = awaken_agent_contract::stable_fingerprint(&(
            &legacy.credential,
            legacy.material_source,
            &legacy.usage,
            &legacy.policy,
            &holder,
            &binding,
        ));
        assert_eq!(
            credential_envelope_payload_fingerprint(&legacy, &holder, &binding),
            legacy_expected,
            "F1/E1"
        );
        let legacy_wire = serde_json::to_value(&legacy).expect("F1 wire");
        assert!(legacy_wire.get("target").is_none(), "F1/E1");

        let first_target = crate::CredentialTarget::new(
            CredentialPurpose::RepositoryTransport,
            crate::repository_transport_audience("https://github.com/awaken/first.git").unwrap(),
        );
        let described = legacy.clone().with_target(first_target);
        let described_wire = serde_json::to_value(&described).expect("F2 wire");
        assert!(described_wire.get("target").is_some(), "F2/E2");
        assert!(described_wire.get("usage").is_some(), "F2/E2");

        let second_target = crate::CredentialTarget::new(
            CredentialPurpose::RepositoryTransport,
            crate::repository_transport_audience("https://git.example.test/awaken/first.git")
                .unwrap(),
        );
        let changed = legacy.with_target(second_target);
        assert_ne!(
            credential_envelope_payload_fingerprint(&described, &holder, &binding),
            credential_envelope_payload_fingerprint(&changed, &holder, &binding),
            "F3/E3"
        );
    }
}
