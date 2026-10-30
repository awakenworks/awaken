// @formal evidence session_resources.external_io_requires_durable_prepared_state
// @formal evidence session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
//! Worker Resource-effect authorization tables, beside their component.

use super::*;

/// Resource-staging authorization cause/effect graph: C1 the Session
/// root is nonterminal; C2 the current live root lease authorizes the asserted
/// owner/incarnation/epoch and monotonic expiry; C3 revision, target kind, and full resolved input/Skill equal
/// the authoritative generation; C4 a replacement is pending. Effects: E1
/// return the root-owned Workspace; E2 stale ownership; E3 invalid frozen
/// input; E4 terminal denial. Active and pending are independently exact
/// retained pairs (their crossover table lives in resource_resume). No Run claim or
/// parallel staging state is synthesized.
///
/// | Rule | C1 | C2 | C3 | C4 | Effect |
/// |---|---|---|---|---|---|
/// | R1 | yes | yes | yes | yes | E1 pending target |
/// | R2 | yes | no | any | any | E2 |
/// | R3 | yes | yes | no | any | E3 |
/// | R4 | no | any | any | any | E4 |
/// | R5 | yes | yes | yes | no | E1 active rebuild target |
/// Renewal regression: R6 same-generation shorter assertion (whether its old
/// timestamp is live or expired) + live renewed root => E1 for every target;
/// R7 missing/expired current root, foreign owner/incarnation/epoch, or an
/// assertion beyond the root expiry => E2. A heartbeat may extend expiry while
/// File/Skill/Memory preparation is in flight; that is not ownership loss.
#[tokio::test]
async fn resource_authority_is_the_exact_pending_session_generation() {
    let lease = crate::session_resource::SessionRealizationLease {
        owner: "worker-resource-memory".into(),
        runtime_incarnation: "worker-resource-memory:1:boot".into(),
        epoch: 7,
        expires_at_unix_ms: u64::MAX - 1,
    };
    let input = crate::session_resource::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from("pending-memory"),
        source: crate::session_resource::ResolvedInputSource::MemoryStore {
            memory_store_id: awaken_resource_contract::MemoryStoreId::from("memory-pending"),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from("memory-pending"),
                version: awaken_resource_contract::ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        },
        mount_path: "/memory/pending".into(),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: None,
    };
    let file = file_resources("file-pending").inputs()[0].clone();
    let skill = skill_resources("skill-pending").skills()[0].clone();
    let desired = crate::session_resource::ResolvedSessionResources::try_new(
        vec![input.clone(), file.clone()],
        vec![skill.clone()],
    )
    .expect("pending Memory, File, and Skill targets");
    let repo: Arc<dyn crate::SessionStore> =
        sqlite_session_repository("resource Memory authority repository").await;
    for (session_id, terminal, active) in [
        ("resource-memory-current", false, false),
        ("resource-memory-active", false, true),
        ("resource-memory-terminal", true, false),
        ("resource-memory-expired", false, false),
        ("resource-memory-unowned", false, false),
    ] {
        let mut session = persisted(
            session_id,
            true,
            if terminal { "terminated" } else { "idle" },
        );
        session.realization = Some(lease.clone());
        if session_id == "resource-memory-expired" {
            session.realization.as_mut().unwrap().expires_at_unix_ms = 1;
        } else if session_id == "resource-memory-unowned" {
            session.realization = None;
        }
        session
            .resources
            .prepare(
                &awaken_session_types::SessionId::from(session_id),
                desired.clone(),
            )
            .expect("prepare exact Memory generation");
        if active {
            session
                .resources
                .commit()
                .expect("commit active generation");
        }
        create(repo.as_ref(), session).await;
    }
    let application = application(repo, Arc::new(RecordingEnvironmentSource::default()));
    let exact = awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
        "resource-memory-current",
        1,
        input.clone(),
        lease.clone(),
    )
    .expect("exact pending Memory intent");
    assert_eq!(
        application
            .resource_effect_authorization()
            .authorize_resource_materialization_intent(&exact)
            .await
            .expect("R1/E1 exact root authority"),
        "workspace",
        "R1/E1"
    );
    let exact_active =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
            "resource-memory-active",
            1,
            input.clone(),
            lease.clone(),
        )
        .expect("exact active rebuild intent");
    assert_eq!(
        application
            .resource_effect_authorization()
            .authorize_resource_materialization_intent(&exact_active)
            .await
            .expect("R5/E1 exact active rebuild authority"),
        "workspace",
        "R5/E1"
    );
    let exact_file =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
            "resource-memory-current",
            1,
            file,
            lease.clone(),
        )
        .expect("exact pending File intent");
    let exact_skill =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new_skill(
            "resource-memory-current",
            1,
            skill.clone(),
            lease.clone(),
        )
        .expect("exact pending Skill intent");
    for intent in [&exact_file, &exact_skill] {
        assert_eq!(
            application
                .resource_effect_authorization()
                .authorize_resource_materialization_intent(intent)
                .await
                .expect("R1/E1 exact non-Memory authority"),
            "workspace",
            "R1/E1"
        );
    }

    for session_id in ["resource-memory-current", "resource-memory-active"] {
        for asserted_expiry in [1, u64::MAX - 2] {
            for target in [&exact, &exact_file, &exact_skill] {
                let mut encoded = serde_json::to_value(target).unwrap();
                encoded["session_id"] = session_id.into();
                encoded["lease"]["expires_at_unix_ms"] = asserted_expiry.into();
                let renewed_intent = serde_json::from_value(encoded).unwrap();
                assert_eq!(
                    application
                        .resource_effect_authorization()
                        .authorize_resource_materialization_intent(&renewed_intent)
                        .await
                        .expect("R6 same-generation renewal keeps the target authorized"),
                    "workspace",
                );
            }
        }
    }
    for (field, value) in [
        ("owner", serde_json::json!("another-worker")),
        ("runtime_incarnation", serde_json::json!("another-boot")),
        ("epoch", serde_json::json!(lease.epoch + 1)),
        ("expires_at_unix_ms", serde_json::json!(u64::MAX)),
    ] {
        let mut encoded = serde_json::to_value(&exact).unwrap();
        encoded["lease"][field] = value;
        let denied = serde_json::from_value(encoded).unwrap();
        assert!(
            matches!(
                application
                    .resource_effect_authorization()
                    .authorize_resource_materialization_intent(&denied)
                    .await,
                Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
            ),
            "R7 rejects forged {field}"
        );
    }
    for session_id in ["resource-memory-expired", "resource-memory-unowned"] {
        let mut encoded = serde_json::to_value(&exact).unwrap();
        encoded["session_id"] = session_id.into();
        encoded["lease"]["expires_at_unix_ms"] = 1.into();
        let denied = serde_json::from_value(encoded).unwrap();
        assert!(
            matches!(
                application
                    .resource_effect_authorization()
                    .authorize_resource_materialization_intent(&denied)
                    .await,
                Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
            ),
            "R7 root must still have a live owner: {session_id}"
        );
    }

    let stale = awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
        "resource-memory-current",
        1,
        input.clone(),
        crate::session_resource::SessionRealizationLease {
            epoch: lease.epoch + 1,
            ..lease.clone()
        },
    )
    .expect("foreign lease intent");
    assert!(
        matches!(
            application
                .resource_effect_authorization()
                .authorize_resource_materialization_intent(&stale)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "R2/E2"
    );

    let wrong_revision =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
            "resource-memory-current",
            2,
            input.clone(),
            lease.clone(),
        )
        .expect("wrong revision remains structurally valid");
    let mut wrong_input = input.clone();
    wrong_input.mount_path = "/memory/foreign".into();
    let wrong_input =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
            "resource-memory-current",
            1,
            wrong_input,
            lease.clone(),
        )
        .expect("foreign input remains structurally valid");
    let mut foreign_skill = skill;
    foreign_skill.version += 1;
    let wrong_skill =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new_skill(
            "resource-memory-current",
            1,
            foreign_skill,
            lease.clone(),
        )
        .expect("foreign Skill remains structurally valid");
    for intent in [&wrong_revision, &wrong_input, &wrong_skill] {
        assert!(
            matches!(
                application
                    .resource_effect_authorization()
                    .authorize_resource_materialization_intent(intent)
                    .await,
                Err(
                    awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                        _
                    )
                )
            ),
            "R3/E3"
        );
    }

    let terminal =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
            "resource-memory-terminal",
            1,
            input,
            lease,
        )
        .expect("terminal-root intent");
    assert!(
        matches!(
            application
                .resource_effect_authorization()
                .authorize_resource_materialization_intent(&terminal)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Terminal)
        ),
        "R4/E4"
    );
}

/// Terminal effect authorization cause/effect graph: C1 the asserted effect
/// lease expired more than 20 seconds ago; C2 the current Session root is the
/// same owner/incarnation/epoch and is either renewed/live or still expired;
/// C3 the asserted epoch is exact or a foreign successor. Effects: E1 an old
/// assertion may start cleanup preparation, terminal Memory, and physical
/// disposal only through its live same-generation current renewal; E2 an
/// unrenewed current root is stale before any new effect or Workspace/provider
/// authority is returned; E3 a foreign epoch remains stale even while the
/// current predecessor is live. Control's current root is the only lease clock;
/// receipt recording remains generation-based so an admitted long preparation
/// may finish after ordinary expiry and durably expose the later disposal.
///
/// | Rule | asserted | current root | Authorized effects |
/// |---|---|---|---|
/// | A1 | exact, expired >20s | same generation renewed/live | E1: preparation + Memory + disposal |
/// | A2 | exact, expired >20s | same generation expired | E2: none; no Workspace/provider authority |
/// | A3 | foreign epoch | predecessor renewed/live | E3: none |
#[tokio::test]
async fn terminal_effect_authorization_requires_current_live_same_generation() {
    let now = awaken_time::system_clock().now().as_epoch_ms();
    let asserted = crate::session_resource::SessionRealizationLease {
        owner: "worker-terminal-authority".into(),
        runtime_incarnation: "worker-terminal-authority:2:boot-current".into(),
        epoch: 29,
        expires_at_unix_ms: now.saturating_sub(20_001),
    };
    let renewed = crate::session_resource::SessionRealizationLease {
        expires_at_unix_ms: now.saturating_add(30_000),
        ..asserted.clone()
    };
    let memory_input = crate::session_resource::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from("terminal-memory"),
        source: crate::session_resource::ResolvedInputSource::MemoryStore {
            memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                "terminal-authority-memory",
            ),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                    "terminal-authority-memory",
                ),
                version: awaken_resource_contract::ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        },
        mount_path: "/memory/terminal-authority".into(),
        access: awaken_resource_contract::ResourceAccess::ReadWrite,
        instructions: None,
    };
    let materialization = awaken_provisioning_contract::MemoryMaterializationEvidence::new(
        "terminal-authority-memory",
        awaken_provisioning_contract::memory_materialization_mount_path(&memory_input.mount_path),
        Vec::new(),
    )
    .expect("terminal authority Memory evidence");
    let terminal_session =
        |session_id: &str,
         realization: Option<crate::session_resource::SessionRealizationLease>| {
            let mut session = registry_race_terminal_session(session_id, realization);
            session.resources = awaken_session_contract::SessionResourceState::from_active(
                crate::session_resource::ResolvedSessionResources::try_new(
                    vec![memory_input.clone()],
                    Vec::new(),
                )
                .expect("terminal authority Memory input"),
            );
            let realization_fingerprint =
                awaken_provisioning_contract::SandboxRealizationFingerprint::from_spec(
                    &awaken_provisioning_contract::SandboxSpec {
                        process_tree_supervision: false,
                        scope: session_id.into(),
                        isolation: awaken_provisioning_contract::IsolationClass::Workdir,
                        environment: None,
                        command: Vec::new(),
                        deny_tool_egress: false,
                        mounts: Vec::new(),
                        env: Vec::new(),
                        packages: Default::default(),
                        network: awaken_provisioning_contract::NetworkPolicy::Unrestricted,
                        outputs_path: "/mnt/session/outputs".into(),
                        requests: Default::default(),
                        limits: Default::default(),
                        filesystem_continuity: Default::default(),
                        control_services: Default::default(),
                        lease_ttl_secs: None,
                    },
                );
            let handle = awaken_provisioning_contract::test_support::fixture_current_handle(
                session_id,
                &awaken_provisioning_contract::test_support::FixtureHandleLocator {
                    outputs_path: "/mnt/session/outputs".into(),
                    physical_incarnation: format!("{session_id}:physical"),
                },
                &realization_fingerprint,
                &asserted
                    .sandbox_effect_fence("create-terminal-authority-memory")
                    .expect("terminal authority Memory creation fence"),
                &[
                    awaken_provisioning_contract::memory_materialization_mount_path(
                        &memory_input.mount_path,
                    ),
                ],
            )
            .with_memory_materializations(vec![materialization.clone()])
            .expect("terminal authority durable Memory evidence");
            session.environment = awaken_session_contract::SessionEnvironmentState::Resident {
                binding: serde_json::to_string(&handle)
                    .expect("terminal authority Environment binding"),
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            };
            session
        };
    let repo = sqlite_session_repository("terminal effect authority Session repository").await;
    create(
        repo.as_ref(),
        terminal_session("terminal-authority-renewed", Some(renewed)),
    )
    .await;
    create(
        repo.as_ref(),
        terminal_session("terminal-authority-expired", Some(asserted.clone())),
    )
    .await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let effect_for =
        |session_id: &str,
         work: awaken_session_realization_contract::SessionTerminalCleanupWork,
         lease: crate::session_resource::SessionRealizationLease| {
            awaken_session_contract::SessionTerminalCleanupEffect::new(
                terminal_prepare_commands(work)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| panic!("{session_id} root preparation command")),
                lease,
            )
        };
    let renewed_work = application
        .terminal_cleanups
        .terminal_cleanup_work(&SessionId::from("terminal-authority-renewed"), &asserted)
        .await
        .expect("A1 exact generation")
        .expect("A1 pending work");
    let renewed_effect = effect_for("terminal-authority-renewed", renewed_work, asserted.clone());
    application
        .terminal_cleanups
        .authorize_terminal_cleanup_effect(&renewed_effect)
        .await
        .expect("A1/E1 old assertion uses current renewal");
    let renewed_memory = awaken_session_contract::terminal_memory_reconciliation_intent(
        &memory_input,
        &materialization,
        &renewed_effect,
    )
    .expect("A1 exact terminal Memory intent");
    application
        .resource_effect_authorization()
        .authorize_terminal_memory_intent(&renewed_memory)
        .await
        .expect("A1/E1 old Memory assertion uses current renewal");
    let renewed_preparation = awaken_session_contract::SessionCleanupPreparation::try_new(
        &renewed_effect,
        renewed_effect
            .sandbox_effect_fence()
            .expect("A1 terminal preparation fence"),
        Vec::new(),
    )
    .expect("A1 terminal preparation");
    application
        .terminal_cleanups
        .record_terminal_cleanup_preparation(&asserted, renewed_preparation)
        .await
        .expect("A1 admitted preparation remains recordable");
    let renewed_disposal = match application
        .terminal_cleanups
        .terminal_cleanup_work(&SessionId::from("terminal-authority-renewed"), &asserted)
        .await
        .expect("A1 exact disposal generation")
        .expect("A1 pending disposal")
        .action
    {
        awaken_session_contract::SessionTerminalCleanupAction::Dispose { command } => {
            awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(
                command,
                asserted.clone(),
            )
        }
        action => panic!("A1 expected disposal action, got {action:?}"),
    };
    assert_eq!(
        application
            .terminal_cleanups
            .authorize_terminal_cleanup_disposal(&renewed_disposal)
            .await
            .expect("A1/E1 old disposal assertion uses current renewal"),
        "workspace",
        "A1/E1 disposal Workspace authority"
    );

    // A2 an expired root cannot mint a fresh assignment time proof. Retained
    // commands still derive from the canonical durable action; constructing
    // that fixture is not runtime admission. Every effect gate below must
    // reject it, while an already-completed preparation may still be recorded.
    assert!(matches!(
        application
            .terminal_cleanups
            .terminal_cleanup_work(&SessionId::from("terminal-authority-expired"), &asserted)
            .await,
        Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
    ));
    let expired_action = repo
        .get(&SessionId::from("terminal-authority-expired"))
        .await
        .unwrap()
        .terminal_cleanup_work_action()
        .unwrap()
        .unwrap();
    let awaken_session_contract::SessionTerminalCleanupAction::Prepare { commands } =
        expired_action
    else {
        panic!("A2 retained preparation command");
    };
    let expired_effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        commands.into_iter().next().unwrap(),
        asserted.clone(),
    );
    assert!(
        matches!(
            application
                .terminal_cleanups.authorize_terminal_cleanup_effect(&expired_effect)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A2/E2"
    );
    let expired_memory = awaken_session_contract::terminal_memory_reconciliation_intent(
        &memory_input,
        &materialization,
        &expired_effect,
    )
    .expect("A2 exact terminal Memory intent");
    assert!(
        matches!(
            application
                .resource_effect_authorization()
                .authorize_terminal_memory_intent(&expired_memory)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A2/E2 terminal Memory"
    );
    let expired_preparation = awaken_session_contract::SessionCleanupPreparation::try_new(
        &expired_effect,
        expired_effect
            .sandbox_effect_fence()
            .expect("A2 terminal preparation fence"),
        Vec::new(),
    )
    .expect("A2 terminal preparation");
    application
        .terminal_cleanups
        .record_terminal_cleanup_preparation(&asserted, expired_preparation)
        .await
        .expect("A2 admitted preparation receipt remains generation-based");
    assert!(matches!(
        application
            .terminal_cleanups
            .terminal_cleanup_work(&SessionId::from("terminal-authority-expired"), &asserted)
            .await,
        Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
    ));
    let expired_disposal = match repo
        .get(&SessionId::from("terminal-authority-expired"))
        .await
        .unwrap()
        .terminal_cleanup_work_action()
        .unwrap()
        .unwrap()
    {
        awaken_session_contract::SessionTerminalCleanupAction::Dispose { command } => {
            awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(
                command,
                asserted.clone(),
            )
        }
        action => panic!("A2 expected disposal action, got {action:?}"),
    };
    assert!(
        matches!(
            application
                .terminal_cleanups.authorize_terminal_cleanup_disposal(&expired_disposal)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A2/E2 terminal disposal returns no Workspace/provider authority"
    );

    let foreign_effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        renewed_effect.command.clone(),
        crate::session_resource::SessionRealizationLease {
            epoch: asserted.epoch + 1,
            ..asserted.clone()
        },
    );
    assert!(
        matches!(
            application
                .terminal_cleanups.authorize_terminal_cleanup_effect(&foreign_effect)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A3/E3"
    );
    let foreign_memory = awaken_session_contract::terminal_memory_reconciliation_intent(
        &memory_input,
        &materialization,
        &foreign_effect,
    )
    .expect("A3 foreign terminal Memory intent");
    assert!(
        matches!(
            application
                .resource_effect_authorization()
                .authorize_terminal_memory_intent(&foreign_memory)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A3/E3 terminal Memory"
    );
    let foreign_disposal = awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(
        renewed_disposal.command,
        foreign_effect.lease,
    );
    assert!(
        matches!(
            application
                .terminal_cleanups.authorize_terminal_cleanup_disposal(&foreign_disposal)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A3/E3 terminal disposal"
    );
}
