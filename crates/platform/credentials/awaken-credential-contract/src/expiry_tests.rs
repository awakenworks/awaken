//! The frozen source expiry and recipient expiry are independent admission fences.

use std::collections::{BTreeMap, BTreeSet};

use crate::*;

fn access() -> (
    CredentialAccess,
    PlaintextHolder,
    CredentialRealizationCapabilities,
) {
    let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "worker.test");
    let access = CredentialAccess::new(
        CredentialRef {
            id: "credential".into(),
            revision: 2,
        },
        CredentialMaterialSource::ControlPlaneReference,
        CredentialUsage::ProviderAdapter,
        CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
    )
    .with_target(CredentialTarget::new(
        CredentialPurpose::ProviderAdapter,
        "provider",
    ));
    let capabilities = CredentialRealizationCapabilities {
        holders: BTreeSet::from([holder.clone()]),
        material_sources: BTreeSet::from([CredentialMaterialSource::ControlPlaneReference]),
        realization_kinds: BTreeSet::from([CredentialRealizationKind::WorkerProviderAdapter]),
        recipient_bound_envelopes: true,
        envelope_required: false,
        extension_consumers: BTreeMap::new(),
        alternatives: Vec::new(),
    };
    (access, holder, capabilities)
}

#[test]
fn source_expiry_admission_is_half_open_and_independent_of_delivery_expiry() {
    let (legacy, holder, capabilities) = access();
    for (expiry, now, accepted) in [
        (None, 10_000, true),
        (Some(5_000), 4_999, true),
        (Some(5_000), 5_000, false),
        (Some(5_000), 5_001, false),
        (Some(0), 0, false),
    ] {
        let mut access = legacy.clone();
        access.expires_at_unix_ms = expiry;
        assert_eq!(
            access
                .admit(
                    &holder,
                    CredentialRealizationKind::WorkerProviderAdapter,
                    &capabilities,
                    now
                )
                .map(|_| ()),
            if accepted {
                Ok(())
            } else {
                Err(CredentialAdmissionError::CredentialExpired)
            }
        );
    }
    let expiring = legacy.with_expiry(5_000);
    let binding = CredentialMaterialBinding::for_target("workspace", &"target", &expiring.usage);
    let issued = credential_envelope_payload_fingerprint(&expiring, &holder, &binding);
    let mut extended = expiring.clone();
    extended.expires_at_unix_ms = Some(6_000);
    assert_ne!(
        issued,
        credential_envelope_payload_fingerprint(&extended, &holder, &binding),
        "changing the source expiry invalidates an issued envelope's AAD"
    );
    assert_ne!(
        issued,
        credential_envelope_payload_fingerprint(
            &expiring.clone().with_issuer_generation(3),
            &holder,
            &binding,
        ),
        "issuer generation is also immutable authenticated data"
    );
    let delivered = expiring.with_envelope(CredentialEnvelope::SealedForWorker {
        envelope_ref: SealedCredentialEnvelopeRef {
            id: "envelope".into(),
            payload_fingerprint: issued,
        },
        recipient: holder.trust_domain.clone(),
        expires_at_unix_ms: 9_000,
    });
    assert_eq!(
        delivered.admit(
            &holder,
            CredentialRealizationKind::WorkerProviderAdapter,
            &capabilities,
            5_000
        ),
        Err(CredentialAdmissionError::CredentialExpired),
        "a live delivery envelope cannot extend the source grant"
    );
}

#[test]
fn preparation_defers_only_positive_source_expiry_without_altering_the_pin() {
    let (access, holder, capabilities) = access();
    let expired = access.with_expiry(5_000).with_issuer_generation(7);
    let original = expired.clone();
    assert!(
        expired
            .admit_for_preparation(
                &holder,
                CredentialRealizationKind::WorkerProviderAdapter,
                &capabilities,
                5_000,
            )
            .is_ok()
    );
    assert_eq!(
        expired, original,
        "preparation never rewrites expiry or issuer identity"
    );
    assert_eq!(
        expired.admit(
            &holder,
            CredentialRealizationKind::WorkerProviderAdapter,
            &capabilities,
            5_000
        ),
        Err(CredentialAdmissionError::CredentialExpired),
        "preparation is not effect authority"
    );
    let foreign = PlaintextHolder::new(PlaintextBoundary::Worker, "foreign.worker");
    assert_eq!(
        expired.admit_for_preparation(
            &foreign,
            CredentialRealizationKind::WorkerProviderAdapter,
            &capabilities,
            5_000,
        ),
        Err(CredentialAdmissionError::HolderNotAllowed)
    );
    let delivered = expired.with_envelope(CredentialEnvelope::SealedForWorker {
        envelope_ref: SealedCredentialEnvelopeRef {
            id: "envelope".into(),
            payload_fingerprint: "fingerprint".into(),
        },
        recipient: holder.trust_domain.clone(),
        expires_at_unix_ms: 5_000,
    });
    assert_eq!(
        delivered.admit_for_preparation(
            &holder,
            CredentialRealizationKind::WorkerProviderAdapter,
            &capabilities,
            5_000,
        ),
        Err(CredentialAdmissionError::EnvelopeExpired),
        "preparation does not relax delivery expiry"
    );
    assert_eq!(
        original.with_expiry(0).admit_for_preparation(
            &holder,
            CredentialRealizationKind::WorkerProviderAdapter,
            &capabilities,
            5_000,
        ),
        Err(CredentialAdmissionError::CredentialExpired),
        "zero is malformed source validity, not a renewable past expiry"
    );
}
