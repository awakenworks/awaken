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
    if !matches!(role, Role::AllInOne | Role::Worker) && request_credential_file {
        return Err(
            "worker_request_credential_file is owned by Worker, not this process role".to_owned(),
        );
    }
    if role == Role::AllInOne && request_credential_file != trust_credentials_file {
        return Err(
            "AllInOne signed Worker transport requires both worker_request_credential_file and worker_trust_credentials_file"
                .to_owned(),
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
        // R2 Coordinator receives only the trust directory; R3 AllInOne owns the
        // paired trust and request views for its one embedded Worker; R4 Control
        // or Worker receiving Coordinator trust -> reject; R5 Coordinator/Control
        // receiving a request signer -> reject; R6 either half of an AllInOne pair
        // -> reject before startup. The pair reuses one transport contract without
        // granting standalone roles another process's authority.
        assert!(
            validate_credential_file_ownership(Role::Worker, true, false).is_ok(),
            "R1"
        );
        assert!(
            validate_credential_file_ownership(Role::Coordinator, false, true).is_ok(),
            "R2"
        );
        assert!(
            validate_credential_file_ownership(Role::AllInOne, true, true).is_ok(),
            "R3"
        );
        assert!(
            validate_credential_file_ownership(Role::Control, false, true).is_err(),
            "R4"
        );
        assert!(
            validate_credential_file_ownership(Role::Worker, false, true).is_err(),
            "R4"
        );
        assert!(
            validate_credential_file_ownership(Role::Coordinator, true, false).is_err(),
            "R5"
        );
        assert!(
            validate_credential_file_ownership(Role::Control, true, false).is_err(),
            "R5"
        );
        assert!(
            validate_credential_file_ownership(Role::AllInOne, true, false).is_err(),
            "R6 request only"
        );
        assert!(
            validate_credential_file_ownership(Role::AllInOne, false, true).is_err(),
            "R6 trust only"
        );
    }
}
