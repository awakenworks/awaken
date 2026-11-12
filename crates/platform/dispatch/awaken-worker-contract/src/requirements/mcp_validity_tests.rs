//! Actual compatibility and assignment kernels, not a parallel selector.

use super::*;
use crate::{WorkerIdentity, WorkerSnapshot, WorkerState, can_assign};
use awaken_agent_contract::credential::{
    CredentialExecutionPolicy, CredentialMaterialSource, CredentialUsage, ModelExposurePolicy,
    PlaintextBoundary, PlaintextHolder,
};

#[test]
fn validity_metadata_requires_new_generic_support_even_when_a_caller_strips_the_cap() {
    let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker");
    for (expiry, generation) in [
        (None, None),
        (Some(10), None),
        (None, Some(1)),
        (Some(10), Some(1)),
    ] {
        let mut access = CredentialAccess::new(
            CredentialRef {
                id: "source".into(),
                revision: 1,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        );
        access.expires_at_unix_ms = expiry;
        access.issuer_generation = generation;
        let needed = expiry.is_some() || generation.is_some();
        let mut required = PlacementRequirements::remote_required();
        required.bind_session_mcp_credentials(Some(vec![
            awaken_credential_contract::McpCredentialRequirement {
                credential: access,
                selected_plaintext_holder: holder.clone(),
            },
        ]));
        assert_eq!(
            required
                .required_capabilities
                .contains(SESSION_MCP_CREDENTIAL_VALIDITY_CAPABILITY),
            needed
        );
        let mut old = WorkerManifest::default();
        old.capacity.max_concurrent = 1;
        old.runtime_protocol = crate::VersionRange::exact(CURRENT_RUNTIME_PROTOCOL_VERSION);
        old.dispatch_contract = crate::VersionRange::exact(CURRENT_CONTRACT_VERSION);
        assert_eq!(
            can_claim(&old, &required).is_ok(),
            !needed,
            "expiry={expiry:?}, generation={generation:?}"
        );
        let snapshot = |manifest: WorkerManifest| WorkerSnapshot {
            identity: WorkerIdentity {
                worker_id: "generic".into(),
                incarnation_id: "boot".into(),
                generation: 1,
            },
            capability_fingerprint: manifest.fingerprint().unwrap(),
            manifest,
            state: WorkerState::Ready,
            in_flight: 0,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Vec::new(),
            expires_at_ms: 100,
        };
        assert_eq!(
            can_assign(&snapshot(old.clone()), &required, None, false, 1).is_ok(),
            !needed
        );
        // Even a hand-built demand cannot hide the metadata from the final
        // immutable compatibility kernel. This token is still not credential access.
        required
            .required_capabilities
            .remove(SESSION_MCP_CREDENTIAL_VALIDITY_CAPABILITY);
        assert_eq!(can_claim(&old, &required).is_ok(), !needed);
        old.capabilities
            .insert(SESSION_MCP_CREDENTIAL_VALIDITY_CAPABILITY.to_owned());
        assert!(can_claim(&old, &required).is_ok());
        assert!(can_assign(&snapshot(old), &required, None, false, 1).is_ok());
    }
    let mut legacy = PlacementRequirements::remote_required();
    legacy.bind_session_mcp_credentials(None);
    assert!(
        !legacy
            .required_capabilities
            .contains(SESSION_MCP_CREDENTIAL_VALIDITY_CAPABILITY)
    );
    legacy.bind_session_mcp_credentials(Some(Vec::new()));
    assert!(
        !legacy
            .required_capabilities
            .contains(SESSION_MCP_CREDENTIAL_VALIDITY_CAPABILITY)
    );
}
