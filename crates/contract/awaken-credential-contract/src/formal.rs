use super::{
    CredentialRealizationSelection, credential_realization_selection,
    durable_material_source_is_reference_only, project_worker_plaintext_holder,
    validate_credential_envelope_issuance_claim,
};

/// The production source enum is the durable credential channel. This proof
/// keeps its complete current domain reference-only; material remains behind
/// the non-serializable resolver result.
#[kani::proof]
fn durable_credential_material_source_has_no_inline_plaintext_class() {
    // Cause/effect graph: C1 selects the Control-plane source; C2 selects the
    // Worker-local source. E1 classifies the selected source as reference-only.
    // The causes are mutually exclusive and exhaustive because the production
    // match has no wildcard. Decision table: R1=C1=>E1; R2=C2=>E1. Adding a
    // source variant breaks compilation until this boundary is reviewed.
    let source = if kani::any::<bool>() {
        super::CredentialMaterialSource::ControlPlaneReference
    } else {
        super::CredentialMaterialSource::WorkerReference
    };

    assert!(durable_material_source_is_reference_only(source));
}

#[kani::proof]
fn environment_credential_custody_selects_exactly_one_authorized_profile() {
    let acp_runtime: bool = kani::any();
    let cloud_environment: bool = kani::any();
    let hosted_profile_installed: bool = kani::any();
    let selection =
        credential_realization_selection(acp_runtime, cloud_environment, hosted_profile_installed);

    assert_eq!(
        selection == CredentialRealizationSelection::SelfHostedAcp,
        acp_runtime
    );
    assert_eq!(
        selection == CredentialRealizationSelection::HostedCloud,
        !acp_runtime && cloud_environment && hosted_profile_installed
    );
    assert_eq!(
        selection == CredentialRealizationSelection::SelfHostedNative,
        !acp_runtime && (!cloud_environment || !hosted_profile_installed)
    );
}

#[kani::proof]
fn credential_envelope_issuance_accepts_exactly_the_complete_claim() {
    let reference_nonempty: bool = kani::any();
    let payload_fingerprint_matches: bool = kani::any();
    let recipient_matches: bool = kani::any();
    let boundary_matches: bool = kani::any();

    let accepted = validate_credential_envelope_issuance_claim(
        reference_nonempty,
        payload_fingerprint_matches,
        recipient_matches,
        boundary_matches,
    )
    .is_ok();
    assert_eq!(
        accepted,
        reference_nonempty && payload_fingerprint_matches && recipient_matches && boundary_matches
    );
}

/// The file-backed Worker resolver preserves the exact opaque trust-domain
/// identity and can project it to no plaintext boundary except Worker.
#[kani::proof]
fn worker_plaintext_holder_projection_is_exact_and_non_widening() {
    let trust_domain_identity = kani::any::<u64>();
    let projected = project_worker_plaintext_holder(trust_domain_identity);

    assert_eq!(projected.boundary, super::PlaintextBoundary::Worker);
    assert_eq!(projected.trust_domain, trust_domain_identity);
    assert_ne!(projected.boundary, super::PlaintextBoundary::Workload);
    assert_ne!(projected.boundary, super::PlaintextBoundary::Platform);
}
