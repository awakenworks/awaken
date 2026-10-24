//! Claim admission of Session MCP credentials through the typed requirements
//! the Session publishes at admission, and the full-decode fallback for rows
//! written before those requirements existed.

use super::*;
use awaken_credential_contract::McpCredentialRequirement;
use awaken_session_configuration_contract::SessionRuntimeProjection;

fn acp_mcp_capabilities(
    selected_holder: &PlaintextHolder,
    backend_ref: &str,
) -> CredentialRealizationCapabilities {
    let mut installed = capabilities(
        selected_holder,
        CredentialRealizationKind::ProcessProtocolField,
    );
    installed.extension_consumers.insert(
        format!(
            "{}{}",
            awaken_credential_contract::ACP_CREDENTIAL_CONSUMER_PREFIX,
            backend_ref
        ),
        BTreeSet::from(["awaken.credential.mcp-process-protocol-field/v1".to_string()]),
    );
    installed
}

/// The frozen projection the Session dispatches, with one MCP stage.
fn session_projection(
    credential: Option<CredentialAccess>,
    selected_plaintext_holder: Option<PlaintextHolder>,
) -> SessionRuntimeProjection {
    let worker = holder(PlaintextBoundary::Worker, "worker-a");
    let mcp_holder = selected_plaintext_holder
        .clone()
        .unwrap_or_else(|| worker.clone());
    SessionRuntimeProjection {
        environment: awaken_session_configuration_contract::EnvironmentSnapshot {
            environment_id: "environment-a".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            self_hosted: true,
            config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                "environment-a@1".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization: awaken_credential_contract::CredentialRealizationProfile {
                inference_holder: worker.clone(),
                mcp_holder,
                resource_holder: worker,
            },
        },
        tools: Some(Default::default()),
        mcp_stages: Some(vec![
            awaken_session_configuration_contract::mcp::StageMcpAttachment {
                workspace_id: "workspace-a".into(),
                generation:
                    awaken_session_configuration_contract::mcp::McpGenerationRef::from_persisted(
                        "session-a".into(),
                        awaken_session_configuration_contract::mcp::McpGeneration::of_bare_target(
                            awaken_session_configuration_contract::mcp::McpAttachmentId(
                                "mcp-a".into(),
                            ),
                            1,
                            &awaken_agent_contract::McpTarget::parse_http(
                                "https://mcp.example.test",
                            )
                            .expect("valid MCP target"),
                        ),
                        "worker-a".into(),
                        1,
                        10_000,
                    )
                    .expect("MCP generation fence epoch is nonzero"),
                realization_id: "realization-a".into(),
                stage_idempotency_key: "stage-a".into(),
                name: "mcp-a".into(),
                target: awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test")
                    .expect("valid MCP target"),
                prompts_as_skills: false,
                credential,
                selected_plaintext_holder,
            },
        ]),
    }
}

fn envelope(projection: &SessionRuntimeProjection) -> crate::SessionRuntimeEnvelope {
    crate::SessionRuntimeEnvelope::new(
        projection
            .encode()
            .expect("Session runtime projection serializes"),
    )
}

/// A row written before the Session published requirements: envelope only.
fn legacy_request(base: &RunDispatch, projection: &SessionRuntimeProjection) -> RunDispatch {
    let mut request = base.clone();
    request.session_runtime = Some(envelope(projection));
    request
}

/// A current admission: the Session validates its projection and publishes
/// the requirements beside the envelope. An invalid projection never becomes
/// a dispatch, so the admission refusal is the outcome.
fn admitted_request(
    base: &RunDispatch,
    projection: &SessionRuntimeProjection,
) -> Result<RunDispatch, DispatchCredentialAdmissionError> {
    let requirements = projection.mcp_credential_requirements()?;
    let mut request = legacy_request(base, projection);
    request.placement.session_mcp_credentials = Some(requirements);
    Ok(request)
}

fn claim(
    request: Result<RunDispatch, DispatchCredentialAdmissionError>,
    installed: &CredentialRealizationCapabilities,
) -> Result<Vec<AttemptCredentialBinding>, DispatchCredentialAdmissionError> {
    compile_attempt_credential_bindings(&request?, installed, 1, 10)
}

fn mcp_access(selected: &PlaintextHolder, usage: CredentialUsage) -> CredentialAccess {
    CredentialAccess::new(
        CredentialRef {
            id: "mcp-credential".into(),
            revision: 3,
        },
        CredentialMaterialSource::ControlPlaneReference,
        usage,
        CredentialExecutionPolicy::exact(selected.clone(), ModelExposurePolicy::Forbidden),
    )
    .with_target(awaken_agent_contract::credential::CredentialTarget::new(
        awaken_agent_contract::credential::CredentialPurpose::McpAuthorization,
        "https://mcp.example.test/",
    ))
}

fn bearer(name: &str, scheme: &str) -> CredentialUsage {
    CredentialUsage::HttpHeader {
        name: name.into(),
        scheme: Some(scheme.into()),
    }
}

/// One claim-admission fixture: the Run, its Session projection, the installed
/// Worker evidence, and the outcome the M-table expects.
struct Fixture {
    rule: &'static str,
    base: RunDispatch,
    projection: SessionRuntimeProjection,
    installed: CredentialRealizationCapabilities,
    expected: Result<Vec<AttemptCredentialBinding>, DispatchCredentialAdmissionError>,
}

/// Every row of the MCP credential claim table.
///
/// | Rule | binding | usage | holder/backend | installed evidence | Effect |
/// | M1 | none | - | - | - | E1 credential-free |
/// | M2 | paired | bearer | Worker | exact relay | E1 legacy relay |
/// | M3 | paired | bearer | Workload/all ACP | same-profile exact marker | E1 client injection |
/// | M4 | unpaired | - | - | - | E2 invalid binding |
/// | M5 | paired | other | Worker | exact relay | E2 invalid usage |
/// | M6 | paired | bearer | Platform | otherwise exact | E2 unsupported holder |
/// | M7 | paired | bearer | Workload/non-ACP | otherwise exact | E2 unsupported backend |
/// | M8 | paired | bearer | Workload/ACP | missing mechanism | E3 holder/mechanism unsupported |
/// | M9 | paired | bearer | Workload/ACP | missing source | E3 source unsupported |
/// | M10 | paired | bearer | Workload/ACP | marker names another backend | E2 unsupported adapter |
/// | M11 | paired | bearer | Workload/ACP | marker/mechanism split across profiles | E2 no cross-profile synthesis |
fn fixtures() -> Vec<Fixture> {
    let worker = holder(PlaintextBoundary::Worker, "worker-a");
    let workload = holder(PlaintextBoundary::Workload, "workload-a");
    let platform = holder(PlaintextBoundary::Platform, "platform-a");
    let worker_installed = capabilities(&worker, CredentialRealizationKind::WorkerRelay);
    let workload_installed = acp_mcp_capabilities(&workload, "acp:test");
    let worker_bearer = mcp_access(&worker, bearer("Authorization", "Bearer"));
    let workload_bearer = mcp_access(&workload, bearer("authorization", "bearer"));
    let client_injection =
        session_projection(Some(workload_bearer.clone()), Some(workload.clone()));
    let mut non_acp = a_request();
    non_acp.activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
            "prov", "model", "genai",
        ));
    let mut missing_kind = workload_installed.clone();
    missing_kind.realization_kinds.clear();
    let mut missing_source = workload_installed.clone();
    missing_source.material_sources.clear();
    let mechanism_only = capabilities(&workload, CredentialRealizationKind::ProcessProtocolField);
    let mut marker_only = CredentialRealizationCapabilities::default();
    marker_only.extension_consumers.insert(
        format!(
            "{}{}",
            awaken_credential_contract::ACP_CREDENTIAL_CONSUMER_PREFIX,
            "acp:test"
        ),
        BTreeSet::from(["awaken.credential.mcp-process-protocol-field/v1".to_string()]),
    );
    let fixture = |rule, base: RunDispatch, projection, installed, expected| Fixture {
        rule,
        base,
        projection,
        installed,
        expected,
    };
    vec![
        fixture(
            "M1",
            a_request(),
            session_projection(None, None),
            worker_installed.clone(),
            Ok(Vec::new()),
        ),
        fixture(
            "M2",
            a_request(),
            session_projection(Some(worker_bearer.clone()), Some(worker.clone())),
            worker_installed.clone(),
            Ok(Vec::new()),
        ),
        fixture(
            "M3",
            a_request(),
            client_injection.clone(),
            workload_installed.clone(),
            Ok(Vec::new()),
        ),
        fixture(
            "M4",
            a_request(),
            session_projection(Some(worker_bearer.clone()), None),
            worker_installed.clone(),
            Err(DispatchCredentialAdmissionError::InvalidSessionMcpCredentialBinding),
        ),
        fixture(
            "M5",
            a_request(),
            session_projection(
                Some(mcp_access(&worker, CredentialUsage::ProviderAdapter)),
                Some(worker.clone()),
            ),
            worker_installed,
            Err(DispatchCredentialAdmissionError::InvalidSessionMcpCredentialUsage),
        ),
        fixture(
            "M6",
            a_request(),
            session_projection(
                Some(mcp_access(&platform, bearer("Authorization", "Bearer"))),
                Some(platform.clone()),
            ),
            capabilities(&platform, CredentialRealizationKind::PlatformRelay),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
        ),
        fixture(
            "M7",
            non_acp,
            client_injection.clone(),
            workload_installed.clone(),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
        ),
        fixture(
            "M8",
            a_request(),
            client_injection.clone(),
            missing_kind,
            Err(DispatchCredentialAdmissionError::SessionAdmission(
                CredentialAdmissionError::HolderUnsupported,
            )),
        ),
        fixture(
            "M9",
            a_request(),
            client_injection.clone(),
            missing_source,
            Err(DispatchCredentialAdmissionError::SessionAdmission(
                CredentialAdmissionError::MaterialSourceUnsupported,
            )),
        ),
        fixture(
            "M10",
            a_request(),
            client_injection.clone(),
            acp_mcp_capabilities(&workload, "acp:other"),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
        ),
        fixture(
            "M11",
            a_request(),
            client_injection,
            CredentialRealizationCapabilities::alternatives([mechanism_only, marker_only]),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
        ),
    ]
}

#[test]
fn session_mcp_credentials_join_the_existing_claim_admission() {
    // Cause/effect graph: C1 credential/holder are absent or paired; C2
    // usage is canonical Authorization Bearer; C3 holder is legacy Worker,
    // ACP Workload, or unsupported Platform; C4 every effective backend is
    // ACP for Workload delivery; C5 one installed profile contains the exact
    // holder/source/ProcessProtocolField plus that backend's declaration.
    // Effects: E1 credential-free, legacy relay, and exact client injection
    // admit without adding an MCP attempt binding; E2 malformed/unsupported
    // requests fail before Worker effects; E3 capability failures retain the
    // common admission error. Constraint: alternative profiles never combine
    // a backend declaration from one adapter with mechanism evidence from
    // another; the Session generation remains holder/receipt authority.
    // The table is in `fixtures`. M4 and M5 are refused when the Session
    // publishes its requirements; every other row is decided at claim by
    // comparing the typed requirement with the installed evidence.
    for fixture in fixtures() {
        assert_eq!(
            claim(
                admitted_request(&fixture.base, &fixture.projection),
                &fixture.installed,
            ),
            fixture.expected,
            "{}",
            fixture.rule
        );
    }
}

#[test]
fn typed_requirement_check_matches_the_legacy_decode_for_every_fixture() {
    // Differential rule D1: for every M-table fixture, admitting through the
    // published typed requirements and admitting a pre-requirement row through
    // the full envelope decode give the identical result, both on the success
    // rows and on the exact refusal.
    for fixture in fixtures() {
        let typed = claim(
            admitted_request(&fixture.base, &fixture.projection),
            &fixture.installed,
        );
        let legacy = claim(
            Ok(legacy_request(&fixture.base, &fixture.projection)),
            &fixture.installed,
        );
        assert_eq!(typed, legacy, "D1 {}", fixture.rule);
        assert_eq!(typed, fixture.expected, "D1 {}", fixture.rule);
    }
}

#[test]
fn published_requirements_are_authoritative_and_legacy_rows_decode_fail_closed() {
    // Cause/effect table: C1 the row carries published requirements or not;
    // C2 its runtime envelope is valid, malformed, missing its Environment or
    // absent; C3 a requirement read back from the row is tampered.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // | T1 | Some([]) | malformed | - | admits: the envelope is never decoded |
    // | T2 | Some([relay]) | malformed | - | admits on the requirement alone |
    // | T3 | Some([]) | absent | - | admits |
    // | T4 | Some([non-Bearer]) | valid | tampered | InvalidSessionMcpCredentialUsage |
    // | T5 | Some([Platform]) | valid | tampered | UnsupportedSessionMcpHolder |
    // | L1 | None | malformed | - | InvalidSessionCredentialProjection |
    // | L2 | None | no Environment | - | InvalidSessionCredentialProjection (full decode) |
    // | L3 | None | absent | - | admits: no Session MCP state |
    let worker = holder(PlaintextBoundary::Worker, "worker-a");
    let platform = holder(PlaintextBoundary::Platform, "platform-a");
    let relay = capabilities(&worker, CredentialRealizationKind::WorkerRelay);
    let requirement =
        |access: CredentialAccess, holder: &PlaintextHolder| McpCredentialRequirement {
            credential: access,
            selected_plaintext_holder: holder.clone(),
        };
    let with = |runtime: Option<&str>, requirements: Option<Vec<McpCredentialRequirement>>| {
        let mut request = a_request();
        request.session_runtime = runtime.map(crate::SessionRuntimeEnvelope::new);
        request.placement.session_mcp_credentials = requirements;
        request
    };
    let admit = |request: RunDispatch| compile_attempt_credential_bindings(&request, &relay, 1, 10);

    assert_eq!(
        admit(with(Some("{"), Some(Vec::new()))),
        Ok(Vec::new()),
        "T1"
    );
    assert_eq!(
        admit(with(
            Some("{"),
            Some(vec![requirement(
                mcp_access(&worker, bearer("Authorization", "Bearer")),
                &worker,
            )]),
        )),
        Ok(Vec::new()),
        "T2"
    );
    assert_eq!(admit(with(None, Some(Vec::new()))), Ok(Vec::new()), "T3");
    let valid = envelope(&session_projection(None, None)).projection_json;
    assert_eq!(
        admit(with(
            Some(&valid),
            Some(vec![requirement(
                mcp_access(&worker, CredentialUsage::ProviderAdapter),
                &worker,
            )]),
        )),
        Err(DispatchCredentialAdmissionError::InvalidSessionMcpCredentialUsage),
        "T4"
    );
    assert_eq!(
        admit(with(
            Some(&valid),
            Some(vec![requirement(
                mcp_access(&platform, bearer("Authorization", "Bearer")),
                &platform,
            )]),
        )),
        Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
        "T5"
    );
    assert!(
        matches!(
            admit(with(Some("{"), None)),
            Err(DispatchCredentialAdmissionError::InvalidSessionCredentialProjection(_))
        ),
        "L1"
    );
    assert!(
        matches!(
            admit(with(Some(r#"{"mcp_stages":[]}"#), None)),
            Err(DispatchCredentialAdmissionError::InvalidSessionCredentialProjection(_))
        ),
        "L2"
    );
    assert_eq!(admit(with(None, None)), Ok(Vec::new()), "L3");
}

#[test]
fn published_requirements_wire_is_additive_to_legacy_placement_rows() {
    // W1 a legacy row has no requirements field and decodes as `None`, and
    // re-encoding it adds no field, so its bytes and identity are unchanged;
    // W2 published requirements, including an empty set, round-trip exactly.
    let legacy = a_request();
    let wire = serde_json::to_value(&legacy).expect("W1 encodes");
    assert!(
        wire.get("placement").is_none(),
        "W1 legacy placement omitted"
    );
    let mut placed = a_request();
    placed.placement = crate::PlacementRequirements::remote_required();
    let placed_wire = serde_json::to_value(&placed).expect("W1 encodes");
    assert!(
        placed_wire["placement"]
            .get("session_mcp_credentials")
            .is_none(),
        "W1 no field without requirements"
    );
    let decoded: RunDispatch = serde_json::from_value(placed_wire).expect("W1 decodes");
    assert_eq!(decoded.placement.session_mcp_credentials, None, "W1");

    let worker = holder(PlaintextBoundary::Worker, "worker-a");
    for requirements in [
        Vec::new(),
        vec![McpCredentialRequirement {
            credential: mcp_access(&worker, bearer("Authorization", "Bearer")),
            selected_plaintext_holder: worker.clone(),
        }],
    ] {
        let mut request = a_request();
        request.placement.session_mcp_credentials = Some(requirements.clone());
        let decoded: RunDispatch =
            serde_json::from_value(serde_json::to_value(&request).expect("W2 encodes"))
                .expect("W2 decodes");
        assert_eq!(
            decoded.placement.session_mcp_credentials,
            Some(requirements),
            "W2"
        );
    }
}
