use super::*;

#[test]
fn cancellation_credential_scope_is_exact_minimal_and_epoch_bound() {
    // Cause-effect graph: C1 operation is Execute/Cancel/TerminalResolution;
    // C2 the selected backend is authenticated Remote, anonymous Remote, or
    // non-Remote; C3 holder/capability admission succeeds; C4 persisted
    // binding matches the current claim epoch. Effects: E1 full execution
    // requirements; E2 exactly one selected A2A transport binding; E3 empty
    // control binding; E4 fail closed before any secret or wire effect.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // |---|---|---|---|---|---|
    // | C1 | Cancel | authenticated Remote | yes | yes | E2 |
    // | C2 | Cancel | anonymous Remote | - | yes | E3 |
    // | C3 | Cancel | non-Remote | - | yes | E3 |
    // | C4 | Cancel | authenticated Remote | no | - | E4 |
    // | C5 | Cancel | authenticated Remote | yes | old/empty | E4 |
    // | C6 | Execute | Remote + provider fallback | relay only | - | E1 error |
    // | C7 | TerminalResolution | any | - | - | E3 |
    // | C8 | TerminalResolution+Cancel flags | any | - | - | E3; terminal wins |
    // Constraints: cancellation never compiles fallback, advisor, Session
    // MCP, provider, or sandbox material; the selected Remote candidate and
    // its full fingerprint still bind credential, holder, usage, and epoch.
    let worker = holder(PlaintextBoundary::Worker, "worker-a");
    let remote_access = CredentialAccess::new(
        CredentialRef {
            id: "remote-cancel".into(),
            revision: 4,
        },
        CredentialMaterialSource::ControlPlaneReference,
        CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        },
        CredentialExecutionPolicy::exact(worker.clone(), ModelExposurePolicy::Forbidden),
    )
    .with_target(awaken_runtime_contract::CredentialTarget::new(
        awaken_runtime_contract::credential::CredentialPurpose::RemoteAgentAuthorization,
        "https://agent.invalid",
    ));
    let remote = candidate_with_access("remote", "", "a2a:https://agent.invalid", remote_access);
    let fallback = provider_candidate(
        "provider-b",
        "model-b",
        "genai",
        "provider-fallback",
        &worker,
    );
    let request = request_with_candidates(remote, vec![fallback], Some(worker.clone()));
    let relay = capabilities(&worker, CredentialRealizationKind::WorkerRelay);

    let exact = compile_claim_credential_bindings(
        &request,
        &relay,
        9,
        10,
        ClaimCredentialOperation::Cancel,
    )
    .expect("C1 exact remote cancel binding");
    assert_eq!(exact.len(), 1, "C1/E2");
    assert_eq!(exact[0].credential.id, "remote-cancel", "C1/E2");
    assert_eq!(exact[0].claim_epoch, 9, "C1/E2");
    assert_eq!(
        exact[0].selected_realization_kind,
        CredentialRealizationKind::WorkerRelay,
        "C1/E2"
    );
    assert!(
        compile_claim_credential_bindings(
            &request,
            &relay,
            9,
            10,
            ClaimCredentialOperation::Execute,
        )
        .is_err(),
        "C6/E1 execution also requires the provider fallback"
    );

    let anonymous = request_with_candidates(
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_remote(
            ModelBinding::new("", "", "a2a:https://agent.invalid"),
            "workspace-a",
            None,
            "sha256:test-agent-card",
        )
        .expect("anonymous remote"),
        Vec::new(),
        None,
    );
    assert!(
        compile_claim_credential_bindings(
            &anonymous,
            &CredentialRealizationCapabilities::default(),
            9,
            10,
            ClaimCredentialOperation::Cancel,
        )
        .expect("C2 anonymous cancellation")
        .is_empty(),
        "C2/E3"
    );
    assert!(
        compile_claim_credential_bindings(
            &a_request(),
            &CredentialRealizationCapabilities::default(),
            9,
            10,
            ClaimCredentialOperation::Cancel,
        )
        .expect("C3 non-remote cancellation")
        .is_empty(),
        "C3/E3"
    );
    assert!(
        compile_claim_credential_bindings(
            &request,
            &CredentialRealizationCapabilities::default(),
            9,
            10,
            ClaimCredentialOperation::Cancel,
        )
        .is_err(),
        "C4/E4 no anonymous downgrade"
    );
    assert_eq!(
        compile_claim_credential_bindings(
            &request,
            &relay,
            0,
            10,
            ClaimCredentialOperation::Cancel,
        ),
        Err(DispatchCredentialAdmissionError::Attempt(
            AttemptCredentialBindingError::InvalidClaimEpoch,
        )),
        "C5/E4"
    );
    assert_eq!(
        verify_claim_credential_bindings(
            &request,
            &relay,
            10,
            10,
            ClaimCredentialOperation::Cancel,
            &exact,
        ),
        Err(DispatchCredentialAdmissionError::ClaimedBindingMismatch),
        "C5/E4 old epoch"
    );
    assert_eq!(
        verify_claim_credential_bindings(
            &request,
            &relay,
            9,
            10,
            ClaimCredentialOperation::Cancel,
            &[],
        ),
        Err(DispatchCredentialAdmissionError::ClaimedBindingMismatch),
        "C5/E4 empty binding"
    );
    assert!(
        compile_claim_credential_bindings(
            &request,
            &relay,
            9,
            10,
            ClaimCredentialOperation::TerminalResolution,
        )
        .expect("C7 terminal repair")
        .is_empty(),
        "C7/E3"
    );
    assert_eq!(
        claim_credential_operation(true, true),
        ClaimCredentialOperation::TerminalResolution,
        "C8 terminal repair owns the claim even if an old cancel bit remains"
    );
    assert_eq!(
        claim_credential_operation(false, true),
        ClaimCredentialOperation::Cancel,
        "C1 cancellation"
    );
    assert_eq!(
        claim_credential_operation(false, false),
        ClaimCredentialOperation::Execute,
        "C6 execution"
    );
}
