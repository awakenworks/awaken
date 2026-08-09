use super::Role;

pub(super) fn validate_credential_file_ownership(
    role: Role,
    request_credential_file: bool,
    trust_credentials_file: bool,
) -> Result<(), String> {
    if !matches!(role, Role::AllInOne | Role::Coordinator) && trust_credentials_file {
        return Err(
            "worker_trust_credentials_file is owned by Coordinator, not this process role"
                .to_owned(),
        );
    }
    if role != Role::Worker && request_credential_file {
        return Err(
            "worker_request_credential_file is owned by Worker, not this process role".to_owned(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_files_follow_process_authority() {
        // Cause/effect decision table: R1 Worker receives only its request signer;
        // R2 Coordinator/AllInOne may receive the trust directory; R3 Control or
        // Worker receiving Coordinator trust -> reject; R4 any non-Worker receiving
        // a request signer -> reject. These rules cover both mutually exclusive
        // credential projections without granting Control Worker-fleet authority.
        assert!(
            validate_credential_file_ownership(Role::Worker, true, false).is_ok(),
            "R1"
        );
        assert!(
            validate_credential_file_ownership(Role::Coordinator, false, true).is_ok(),
            "R2"
        );
        assert!(
            validate_credential_file_ownership(Role::AllInOne, false, true).is_ok(),
            "R2"
        );
        assert!(
            validate_credential_file_ownership(Role::Control, false, true).is_err(),
            "R3"
        );
        assert!(
            validate_credential_file_ownership(Role::Worker, false, true).is_err(),
            "R3"
        );
        assert!(
            validate_credential_file_ownership(Role::Coordinator, true, false).is_err(),
            "R4"
        );
    }
}
