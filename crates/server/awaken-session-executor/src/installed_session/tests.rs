use super::*;
use awaken_session_mcp::{McpGenerationProjection, McpProjectionState};

#[test]
fn dispatch_resource_manifest_has_one_session_generation_selector() {
    // Cause/effect graph: C1 the slot is ordinary/prepared Session; C2 a
    // desired transition is absent/present; C3 active is absent/present.
    // Effects: E1 ordinary dispatch selects active only; E2 prepared
    // dispatch selects desired before physical publication; E3 desired
    // wins over stale active; E4 a legacy Session falls back to active; E5
    // selection never publishes or replaces active.
    //
    // | Rule | C1       | C2   | C3   | Selected | Active after |
    // | S1   | ordinary | A->B | A    | A        | A            |
    // | S2   | Session  | 0->B | none | B        | none         |
    // | S3   | Session  | A->B | A    | B        | A            |
    // | S4   | Session  | none | A    | A        | A            |
    let manifest = |revision| {
        crate::session_resource::SessionResourceManifest::at_revision(
            "workspace-a",
            revision,
            crate::session_resource::ResolvedSessionResources::default(),
        )
    };
    for (rule, session_dispatch, active, desired, expected) in [
        ("S1", false, Some(4), Some(7), 4),
        ("S2", true, None, Some(7), 7),
        ("S3", true, Some(4), Some(7), 7),
        ("S4", true, Some(4), None, 4),
    ] {
        let active = active.map(manifest);
        let transition = desired.map(|revision| {
            crate::session_resource::SessionResourceTransition::new(
                active.clone().unwrap_or_else(|| manifest(0)),
                manifest(revision),
            )
            .expect("test transition belongs to one Workspace")
        });
        let slot = InstalledSession {
            realization: SessionRealization {
                manifest: active.clone(),
                resource_transition: transition,
                ..Default::default()
            },
            run: SessionRunBinding {
                session_dispatch,
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            slot.dispatch_resource_manifest()
                .unwrap_or_else(|| panic!("{rule} selected manifest"))
                .revision,
            expected,
            "{rule} selection",
        );
        assert_eq!(
            slot.realization.manifest, active,
            "{rule}/E5 active is unchanged"
        );
    }
}

#[test]
fn environment_only_session_lease_remains_supervised() {
    // FMECA/causal graph: C1 Environment startup outlives the initial lease;
    // C2 the Session has no MCP projection; C3 supervision filters on MCP.
    // E1 C1+C2 must still expose the lease for renewal, otherwise C3 lets
    // the durable fence expire before the Environment receipt can commit.
    let slots = InstalledSessions::default();
    slots.update_realization("environment-only", |r| {
        assert!(!r.mcp.has_frozen_attachment(), "C2");
        r.authority = crate::installed_session::SessionRealizationAuthority::Live(
            test_observed_lease(crate::session_resource::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms()
                    + 30_000,
            }),
        );
    });

    let leases = slots.realization_leases();
    assert_eq!(leases.len(), 1, "E1");
    assert_eq!(leases[0].0, "environment-only", "E1");
}

#[tokio::test]
async fn unpolled_staging_future_drop_releases_drain_without_false_proof() {
    // Cause/effect rule U1: C1 the canonical Staging projection is already
    // installed; C2 its spawned Future captures the activity guard but is
    // dropped before first poll; C3 drain has already claimed that exact
    // owner. Effects: E1 drain remains pending before C2, E2 Future drop
    // completes the same activity, and E3 the retry emits exact Removed
    // proof only after the unstarted spawn owner is known to be gone.
    let host = (crate::HostDependencies::ephemeral(
        std::sync::Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let generation =
        crate::test_mcp::mcp_fence("unpolled-staging", "docs", 1, "runtime-1", 1, u64::MAX);
    let request = crate::session_mcp::StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation.clone(),
        realization_id: "realize-1".into(),
        stage_idempotency_key: "stage-1".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test").unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let staging = awaken_session_mcp::McpStagingActivity::default();
    let guard = staging.guard();
    let unpolled = async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    };
    host.insert_mcp_projection(McpGenerationProjection {
        receipt: crate::session_mcp::McpRealizationReceipt {
            generation: generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: None,
            actual_realization_kind: None,
            receipt_fingerprint: request.fingerprint(),
        },
        request,
        server: None,
        native_wiring: None,
        mcp_process: None,
        staging: Some(staging),
        drain: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        state: McpProjectionState::Staging,
    })
    .unwrap();

    let drain_host = host.clone();
    let drain_generation = generation.clone();
    let draining = tokio::spawn(async move {
        drain_host
            .drain_mcp_projections("unpolled-staging", &[drain_generation])
            .await
    });
    loop {
        if host
            .mcp_projection(&generation)
            .is_some_and(|projection| projection.state == McpProjectionState::Draining)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(!draining.is_finished(), "U1/E1 no early proof");

    drop(unpolled);
    let proof = tokio::time::timeout(std::time::Duration::from_millis(50), draining)
        .await
        .expect("U1/E2 unpolled Future drop releases drain")
        .expect("U1/E2 drain task remains owned")
        .expect("U1/E3 exact drain succeeds");
    assert_eq!(
        proof.generations.as_slice(),
        std::slice::from_ref(&generation),
        "U1/E3"
    );
    assert!(
        host.mcp_projection(&generation).is_some_and(|projection| {
            projection.state == McpProjectionState::Removed
                && projection.mcp_process.is_none()
                && projection.staging.is_none()
        }),
        "U1/E3 Removed only after the captured owner drops"
    );
}

#[test]
fn mcp_quiescence_admission_reopens_only_from_post_suspend_authority() {
    // Cause graph: C1 exact suspend/source/generation closes admission; C2
    // the same close replays or conflicts; C3 Control projects the old
    // Resident, a wrong generation, an exact restored Resident, or an
    // explicit source-free Unmaterialized state. C4 the old Environment
    // owner is still pending or is exactly Vacant. Effects: E1 close once;
    // E2 reject conflict and retain fence; E3 stale/early projections stay
    // closed; E4 exact restore/fresh-generation authority reopens once.
    //
    // | Rule | Close | Projected Environment | Effect |
    // |---|---|---|---|
    // | F1 | exact first/replay | none | E1 / idempotent |
    // | F2 | foreign | none | E2 |
    // | F3 | exact | old Resident / wrong generation | E3 |
    // | F4 | exact | same generation + new effect/binding | E4 |
    // | F5 | exact | Unmaterialized + pending source owner | E3 |
    // | F6 | exact | Unmaterialized + Vacant + no local effects | E4 |
    let slots = InstalledSessions::default();
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from("mcp-admission"),
        1,
        100,
        "environment",
        "base",
    );
    let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace",
        &awaken_session_types::SessionId::from("mcp-admission"),
        "suspend",
        &generation,
        4,
        None,
        None,
    );
    let fence = awaken_session_mcp::McpQuiescenceAdmissionFence::new(
        &operation,
        "source-effect",
        "source-binding",
        &generation,
    );
    assert!(
        slots
            .close_mcp_realization_admission("mcp-admission", fence.clone())
            .unwrap(),
        "F1/E1"
    );
    assert!(
        !slots
            .close_mcp_realization_admission("mcp-admission", fence.clone())
            .unwrap(),
        "F1 replay"
    );
    let mut foreign = fence.clone();
    foreign.activity_epoch += 1;
    assert!(
        slots
            .close_mcp_realization_admission("mcp-admission", foreign)
            .is_err(),
        "F2/E2"
    );
    let old_resident = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: "source-binding".into(),
        effect_id: Some("source-effect".into()),
        generation: Some(generation.clone()),
        idle_since_unix_ms: None,
    };
    assert!(
        !slots.update("mcp-admission", |slot| {
            slot.realization.environment_owner = SessionEnvironmentOwner::Preparing(
                SessionEnvironmentPreparation::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: "source-effect".into(),
                        generation: generation.clone(),
                    },
                    binding: "source-binding".into(),
                },
            );
            slot.reopen_mcp_realization_admission_from_projection(&old_resident)
        }),
        "F3/E3 old Resident"
    );
    let wrong_generation = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: "restored-binding".into(),
        effect_id: Some("restore-effect".into()),
        generation: Some(awaken_session_state_contract::SandboxGeneration::new(
            &awaken_session_types::SessionId::from("mcp-admission"),
            2,
            100,
            "environment",
            "base",
        )),
        idle_since_unix_ms: None,
    };
    assert!(
        !slots.update("mcp-admission", |slot| {
            slot.realization.environment_owner = SessionEnvironmentOwner::Preparing(
                SessionEnvironmentPreparation::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: "restore-effect".into(),
                        generation: wrong_generation.generation().unwrap().clone(),
                    },
                    binding: "restored-binding".into(),
                },
            );
            slot.reopen_mcp_realization_admission_from_projection(&wrong_generation)
        }),
        "F3/E3 wrong generation"
    );
    let restored = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: "restored-binding".into(),
        effect_id: Some("restore-effect".into()),
        generation: Some(generation),
        idle_since_unix_ms: None,
    };
    assert!(
        slots.update("mcp-admission", |slot| {
            slot.realization.environment_owner = SessionEnvironmentOwner::Preparing(
                SessionEnvironmentPreparation::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: "restore-effect".into(),
                        generation: restored.generation().unwrap().clone(),
                    },
                    binding: "restored-binding".into(),
                },
            );
            slot.reopen_mcp_realization_admission_from_projection(&restored)
        }),
        "F4/E4"
    );
    assert!(slots.mcp_realization_admitted("mcp-admission"), "F4/E4");

    slots
        .close_mcp_realization_admission("mcp-admission", fence.clone())
        .unwrap();
    assert!(
        !slots.update("mcp-admission", |slot| {
            slot.realization.environment_owner = SessionEnvironmentOwner::Preparing(
                SessionEnvironmentPreparation::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: fence.source_effect_id.clone(),
                        generation: fence.generation.clone(),
                    },
                    binding: fence.source_binding.clone(),
                },
            );
            slot.reopen_mcp_realization_admission_from_projection(
                &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
            )
        }),
        "F5/E3 expiry before source disposal"
    );
    assert!(
        slots.update("mcp-admission", |slot| {
            slot.realization.environment_owner = SessionEnvironmentOwner::Vacant;
            slot.reopen_mcp_realization_admission_from_projection(
                &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
            )
        }),
        "F6/E4 exact source-free projection"
    );
    assert!(slots.mcp_realization_admitted("mcp-admission"), "F6/E4");
}

#[test]
fn frozen_inputs_keep_their_agent_publication() {
    // Decision table: C1 a publication is retained; C2 one is offered; C3 the
    // offer equals the retained one. Only a different offer against a retained
    // publication is refused (fail closed); every other row admits.
    // | P1 | none | none | -   | admit  |
    // | P2 | none | some | -   | admit  |
    // | P3 | some | none | -   | admit  |
    // | P4 | some | some | yes | admit  |
    // | P5 | some | some | no  | refuse |
    let retained = crate::host::test_activation("t", "r1").snapshot;
    let other = crate::host::test_activation("t", "r2").snapshot;
    let empty = FrozenSessionInputs::default();
    assert!(empty.validate_publication(None).is_ok(), "P1");
    assert!(empty.validate_publication(Some(&other)).is_ok(), "P2");
    let frozen = FrozenSessionInputs {
        published_snapshot: Some(retained.clone()),
        ..FrozenSessionInputs::default()
    };
    assert!(frozen.validate_publication(None).is_ok(), "P3");
    assert!(frozen.validate_publication(Some(&retained)).is_ok(), "P4");
    let refused = frozen.validate_publication(Some(&other)).unwrap_err();
    assert_eq!(refused.kind, crate::HostErrorKind::Internal, "P5");
}
