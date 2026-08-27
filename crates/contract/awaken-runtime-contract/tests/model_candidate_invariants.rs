use awaken_runtime_contract::resolved::{
    AcpExecutionProfile, BackendModelSelection, ModelBinding, ModelProvisioning,
    ProviderExecutionProfile, ResolvedModelCandidate, UnspecifiedReasoning,
};
use awaken_runtime_contract::{CredentialRef, InferenceEndpoint};

fn endpoint() -> InferenceEndpoint {
    InferenceEndpoint {
        adapter_kind: "openai".into(),
        api_dialect: "chat_completions".into(),
        base_url: "https://provider.example/v1".into(),
        upstream_model: "model-a".into(),
        processing_placement: None,
    }
}

fn acp_profile() -> AcpExecutionProfile {
    AcpExecutionProfile {
        capability_fingerprint: "sha256:capability".into(),
        capability_adapter_version: "adapter-v1".into(),
        session_configuration: Default::default(),
    }
}

fn credential() -> CredentialRef {
    CredentialRef {
        id: "credential-a".into(),
        revision: 1,
    }
}

fn assert_rejected(binding: ModelBinding, provisioning: ModelProvisioning, expected: &'static str) {
    assert_eq!(
        ResolvedModelCandidate::try_from_parts(binding, provisioning)
            .expect_err("the decision-table row must be rejected")
            .to_string(),
        expected,
    );
}

/// Cause/effect graph: the executor backend chooses exactly one provisioning
/// family; that family then determines whether the model, route, capability,
/// and security coordinates are required. Construction is the only transition
/// into the executable-candidate state.
///
/// Decision table:
/// | Rule | backend | provisioning | additional condition | effect |
/// |---|---|---|---|---|
/// | B1 | exact ACP | BackendOwned | Default + empty model + capability pin | accept |
/// | B2 | exact ACP | BackendOwned | Exact + canonical model + capability pin | accept |
/// | B3 | Native | BackendOwned | any | reject |
/// | B4 | exact ACP | BackendOwned | model policy disagrees with model | reject |
/// | P1 | Native | Provider | complete route, no ACP profile | accept |
/// | P2a | exact ACP | Provider | complete route, ACP profile, provider default | accept |
/// | P2b | exact ACP | Provider | complete route, ACP profile, explicit reasoning policy | accept |
/// | P3 | Native/ACP/A2A | Provider | backend/profile family disagrees | reject |
/// | P4 | Native/ACP | Provider | any required route coordinate is non-canonical | reject |
/// | R1 | exact A2A | Remote | empty model and security fingerprint | accept |
/// | R2 | other | Remote | any | reject |
/// | R3 | exact A2A | Remote | local model or missing security proof | reject |
#[test]
fn executable_candidate_construction_is_a_closed_decision_table() {
    assert!(
        ResolvedModelCandidate::try_backend_owned(
            ModelBinding::new("identity-a", "", "acp:codex"),
            credential(),
            BackendModelSelection::Default,
            "adapter-v1",
            "sha256:capability",
            Default::default(),
        )
        .is_ok(),
        "B1",
    );
    assert!(
        ResolvedModelCandidate::try_backend_owned(
            ModelBinding::new("identity-a", "model-a", "acp:codex"),
            credential(),
            BackendModelSelection::Exact,
            "adapter-v1",
            "sha256:capability",
            Default::default(),
        )
        .is_ok(),
        "B2",
    );
    assert_rejected(
        ModelBinding::new("identity-a", "", "native"),
        ModelProvisioning::BackendOwned {
            credential: credential(),
            model_selection: BackendModelSelection::Default,
            acp: acp_profile(),
        },
        "backend-owned provisioning requires an exact ACP backend",
    );
    for (selection, model) in [
        (BackendModelSelection::Default, "model-a"),
        (BackendModelSelection::Exact, ""),
        (BackendModelSelection::Exact, " model-a"),
    ] {
        assert_rejected(
            ModelBinding::new("identity-a", model, "acp:codex"),
            ModelProvisioning::BackendOwned {
                credential: credential(),
                model_selection: selection,
                acp: acp_profile(),
            },
            "backend model selection and model reference are incoherent",
        );
    }
    assert_rejected(
        ModelBinding::new("identity-a", "", "acp:codex"),
        ModelProvisioning::BackendOwned {
            credential: credential(),
            model_selection: BackendModelSelection::Default,
            acp: AcpExecutionProfile {
                capability_fingerprint: String::new(),
                ..acp_profile()
            },
        },
        "ACP provisioning requires an exact capability pin",
    );

    assert!(
        ResolvedModelCandidate::try_provider(
            ModelBinding::new("identity-a", "model-a", "native"),
            "provider@1",
            "route@1",
            "workspace-a",
            None,
            endpoint(),
        )
        .is_ok(),
        "P1",
    );
    assert!(
        ResolvedModelCandidate::try_provider_with_acp(
            ModelBinding::new("identity-a", "model-a", "acp:claude"),
            "provider@1",
            "route@1",
            "workspace-a",
            None,
            endpoint(),
            acp_profile(),
        )
        .is_ok(),
        "P2",
    );
    assert!(
        ResolvedModelCandidate::try_provider_with_profile(
            ModelBinding::new("identity-a", "model-a", "acp:claude"),
            "provider@1",
            "route@1",
            "workspace-a",
            None,
            endpoint(),
            ProviderExecutionProfile {
                unspecified_reasoning: UnspecifiedReasoning::Disabled,
                acp: Some(acp_profile()),
            },
        )
        .is_ok(),
        "P2b",
    );
    assert_rejected(
        ModelBinding::new("identity-a", "model-a", "acp:claude"),
        ModelProvisioning::Provider {
            provider_ref: "provider@1".into(),
            route_ref: "route@1".into(),
            scope_id: "workspace-a".into(),
            credential: None,
            endpoint: Box::new(endpoint()),
            unspecified_reasoning: Default::default(),
            acp: None,
        },
        "provider provisioning and executor backend are incoherent",
    );
    assert_rejected(
        ModelBinding::new("identity-a", "model-a", "native"),
        ModelProvisioning::Provider {
            provider_ref: "provider@1".into(),
            route_ref: "route@1".into(),
            scope_id: "workspace-a".into(),
            credential: None,
            endpoint: Box::new(endpoint()),
            unspecified_reasoning: Default::default(),
            acp: Some(Box::new(acp_profile())),
        },
        "provider provisioning and executor backend are incoherent",
    );
    assert_rejected(
        ModelBinding::new("identity-a", "model-a", "a2a:https://agent.example"),
        ModelProvisioning::Provider {
            provider_ref: "provider@1".into(),
            route_ref: "route@1".into(),
            scope_id: "workspace-a".into(),
            credential: None,
            endpoint: Box::new(endpoint()),
            unspecified_reasoning: Default::default(),
            acp: None,
        },
        "provider provisioning and executor backend are incoherent",
    );
    assert_rejected(
        ModelBinding::new("identity-a", "model-a", "native"),
        ModelProvisioning::Provider {
            provider_ref: " provider@1".into(),
            route_ref: "route@1".into(),
            scope_id: "workspace-a".into(),
            credential: None,
            endpoint: Box::new(endpoint()),
            unspecified_reasoning: Default::default(),
            acp: None,
        },
        "provider provisioning requires complete canonical route coordinates",
    );

    assert!(
        ResolvedModelCandidate::try_remote(
            ModelBinding::new("agent-a", "", "a2a:https://agent.example"),
            "workspace-a",
            None,
            "sha256:agent-card",
        )
        .is_ok(),
        "R1",
    );
    assert_rejected(
        ModelBinding::new("agent-a", "", "native"),
        ModelProvisioning::Remote {
            scope_id: "workspace-a".into(),
            credential: None,
            security_fingerprint: "sha256:agent-card".into(),
        },
        "remote provisioning requires an exact A2A backend and no local model",
    );
    assert_rejected(
        ModelBinding::new("agent-a", "model-a", "a2a:https://agent.example"),
        ModelProvisioning::Remote {
            scope_id: "workspace-a".into(),
            credential: None,
            security_fingerprint: "sha256:agent-card".into(),
        },
        "remote provisioning requires an exact A2A backend and no local model",
    );
    assert_rejected(
        ModelBinding::new("agent-a", "", "a2a:https://agent.example"),
        ModelProvisioning::Remote {
            scope_id: "workspace-a".into(),
            credential: None,
            security_fingerprint: String::new(),
        },
        "remote provisioning requires an exact security fingerprint",
    );
}

fn assert_wire_rejected(
    candidate: &ResolvedModelCandidate,
    mutate: impl FnOnce(&mut serde_json::Value),
) {
    let mut wire = serde_json::to_value(candidate).expect("serialize valid candidate");
    mutate(&mut wire);
    assert!(
        serde_json::from_value::<ResolvedModelCandidate>(wire).is_err(),
        "persisted bytes cannot bypass candidate construction",
    );
}

/// Persistence is an untrusted constructor. Causes: one valid candidate or one
/// invariant-bearing wire field changed to an invalid value. Effects: the valid
/// value round-trips elsewhere, while every changed value fails before an
/// executable aggregate can enter memory. Constraint/K: custom Deserialize
/// delegates to the same closed constructor as typed creation. Decision rule:
/// P4 incomplete Provider route => reject; the other rows follow B3/B4/P3/R2/R3.
#[test]
fn deserialization_cannot_bypass_candidate_invariants() {
    let backend_owned = ResolvedModelCandidate::try_backend_owned(
        ModelBinding::new("identity-a", "", "acp:codex"),
        credential(),
        BackendModelSelection::Default,
        "adapter-v1",
        "sha256:capability",
        Default::default(),
    )
    .unwrap();
    assert_wire_rejected(&backend_owned, |wire| wire["backend_ref"] = "native".into());
    assert_wire_rejected(&backend_owned, |wire| wire["model_ref"] = "model-a".into());
    assert_wire_rejected(&backend_owned, |wire| {
        wire["provisioning"]["capability_fingerprint"] = "".into();
    });

    let provider = ResolvedModelCandidate::try_provider(
        ModelBinding::new("identity-a", "model-a", "native"),
        "provider@1",
        "route@1",
        "workspace-a",
        None,
        endpoint(),
    )
    .unwrap();
    assert_wire_rejected(&provider, |wire| wire["backend_ref"] = "acp:claude".into());
    assert_wire_rejected(&provider, |wire| {
        wire["provisioning"]["route_ref"] = "".into();
    });
    assert_wire_rejected(&provider, |wire| {
        wire["provisioning"]["endpoint"]["api_dialect"] = "".into();
    });
    assert_wire_rejected(&provider, |wire| {
        wire["provisioning"]["endpoint"]["upstream_model"] = "".into();
    });

    let acp_provider = ResolvedModelCandidate::try_provider_with_acp(
        ModelBinding::new("identity-a", "model-a", "acp:claude"),
        "provider@1",
        "route@1",
        "workspace-a",
        None,
        endpoint(),
        acp_profile(),
    )
    .unwrap();
    assert_wire_rejected(&acp_provider, |wire| {
        wire["provisioning"].as_object_mut().unwrap().remove("acp");
    });

    let remote = ResolvedModelCandidate::try_remote(
        ModelBinding::new("agent-a", "", "a2a:https://agent.example"),
        "workspace-a",
        None,
        "sha256:agent-card",
    )
    .unwrap();
    assert_wire_rejected(&remote, |wire| wire["model_ref"] = "model-a".into());
    assert_wire_rejected(&remote, |wire| {
        wire["provisioning"]["security_fingerprint"] = "".into();
    });
}
