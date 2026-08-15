use super::{
    CredentialRealizationSelection, credential_realization_selection,
    project_worker_plaintext_holder, validate_credential_envelope_issuance_claim,
};

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
