use super::{
    CredentialRealizationSelection, credential_realization_selection,
    validate_credential_envelope_issuance_claim,
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
