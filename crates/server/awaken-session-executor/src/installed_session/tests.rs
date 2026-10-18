use super::*;
use crate::environment_owner::*;
use awaken_session_mcp::{McpGenerationProjection, McpProjectionState};
use awaken_session_realization::test_support::test_observed_lease;

#[test]
fn dispatch_resource_manifest_has_one_session_generation_selector() {
    // Ownership move: ResourceStagingState selects from the same locked slot;
    // its input is the existing Run dispatch bit, never a second stored flag.
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
        let mut realization = SessionRealization::default();
        realization.staging_mut().manifest = active.clone();
        realization.staging_mut().transition = transition;
        let slot = InstalledSession {
            realization,
            run: SessionRunBinding {
                session_dispatch,
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            slot.realization
                .staging()
                .run_manifest(slot.run.session_dispatch)
                .unwrap_or_else(|| panic!("{rule} selected manifest"))
                .revision,
            expected,
            "{rule} selection",
        );
        assert_eq!(
            slot.realization.staging().manifest,
            active,
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
    let slots = Arc::new(InstalledSessions::default());
    slots.install_realization("environment-only", |r| {
        assert!(!r.staging().mcp.has_frozen_attachment(), "C2");
        r.execution_mut().authority = crate::realization_state::SessionRealizationAuthority::Live(
            test_observed_lease(crate::session_resource::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: awaken_session_realization::test_support::wall_clock_now_ms()
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
    host.run_assembly
        .insert_mcp_projection(McpGenerationProjection {
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
            .run_assembly
            .drain_mcp_projections("unpolled-staging", &[drain_generation])
            .await
    });
    loop {
        if host
            .run_assembly
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
        host.run_assembly
            .mcp_projection(&generation)
            .is_some_and(|projection| {
                projection.state == McpProjectionState::Removed
                    && projection.mcp_process.is_none()
                    && projection.staging.is_none()
            }),
        "U1/E3 Removed only after the captured owner drops"
    );
}

#[test]
fn mcp_quiescence_admission_reopens_only_from_post_suspend_authority() {
    // Ownership move: SessionRealization receives the Run context-presence bit
    // from this same slot lock; it retains the original MCP-local-effects and
    // durable-projection checks. Moving the code must not add a reopen path.
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
    let slots = Arc::new(InstalledSessions::default());
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
    slots.install("mcp-admission");
    assert!(
        slots
            .realizations()
            .close_mcp_realization_admission("mcp-admission", fence.clone())
            .unwrap(),
        "F1/E1"
    );
    assert!(
        !slots
            .realizations()
            .close_mcp_realization_admission("mcp-admission", fence.clone())
            .unwrap(),
        "F1 replay"
    );
    let mut foreign = fence.clone();
    foreign.activity_epoch += 1;
    assert!(
        slots
            .realizations()
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
            slot.realization.environment_mut().owner = SessionEnvironmentOwner::Preparing(
                SessionEnvironmentPreparation::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: "source-effect".into(),
                        generation: generation.clone(),
                    },
                    binding: "source-binding".into(),
                },
            );
            slot.realization
                .reopen_mcp_realization_admission_from_projection(
                    &old_resident,
                    slot.run.context.is_some(),
                )
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
            slot.realization.environment_mut().owner = SessionEnvironmentOwner::Preparing(
                SessionEnvironmentPreparation::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: "restore-effect".into(),
                        generation: wrong_generation.generation().unwrap().clone(),
                    },
                    binding: "restored-binding".into(),
                },
            );
            slot.realization
                .reopen_mcp_realization_admission_from_projection(
                    &wrong_generation,
                    slot.run.context.is_some(),
                )
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
            slot.realization.environment_mut().owner = SessionEnvironmentOwner::Preparing(
                SessionEnvironmentPreparation::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: "restore-effect".into(),
                        generation: restored.generation().unwrap().clone(),
                    },
                    binding: "restored-binding".into(),
                },
            );
            slot.realization
                .reopen_mcp_realization_admission_from_projection(
                    &restored,
                    slot.run.context.is_some(),
                )
        }),
        "F4/E4"
    );
    assert!(
        slots
            .realizations()
            .mcp_realization_admitted("mcp-admission")
            .expect("installed Sessions"),
        "F4/E4"
    );

    slots
        .realizations()
        .close_mcp_realization_admission("mcp-admission", fence.clone())
        .unwrap();
    assert!(
        !slots.update("mcp-admission", |slot| {
            slot.realization.environment_mut().owner = SessionEnvironmentOwner::Preparing(
                SessionEnvironmentPreparation::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: fence.source_effect_id.clone(),
                        generation: fence.generation.clone(),
                    },
                    binding: fence.source_binding.clone(),
                },
            );
            slot.realization
                .reopen_mcp_realization_admission_from_projection(
                    &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
                    slot.run.context.is_some(),
                )
        }),
        "F5/E3 expiry before source disposal"
    );
    assert!(
        slots.update("mcp-admission", |slot| {
            slot.realization.environment_mut().owner = SessionEnvironmentOwner::Vacant;
            slot.realization
                .reopen_mcp_realization_admission_from_projection(
                    &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
                    slot.run.context.is_some(),
                )
        }),
        "F6/E4 exact source-free projection"
    );
    assert!(
        slots
            .realizations()
            .mcp_realization_admitted("mcp-admission")
            .expect("installed Sessions"),
        "F6/E4"
    );
}

#[test]
fn frozen_inputs_keep_their_agent_publication() {
    // This is also the complete-projection installer's canonical preflight law;
    // it reads the same immutable FrozenSessionInputs as Environment validation
    // and rechecks under final publication. The adapter retains its old error
    // text; no separate equality predicate or write is permitted at preflight.
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
    assert_eq!(
        refused.disposition(),
        crate::realization_error::Disposition::Terminal,
        "P5"
    );
}

#[tokio::test]
async fn poisoned_installed_sessions_refuse_source_actions_without_callbacks_or_writes() {
    // Cause/effect graph S4: C1 the actual Slot mutex is poisoned by a caught
    // callback panic; C2 target is present/absent; C3 entry is Source.visit
    // (create=false/true), named publication, or legacy read/modify/update.
    // E1 Source denies with the typed poisoned registry lock; E2 named action
    // returns the typed Poisoned realization error (retryable) without panic;
    // E3 legacy retains its exact panic prefix and actual std PoisonError Debug;
    // E4 callback0; E5 creation0 and all Frozen/Resource/Memory/Skill/key/Run
    // fields unchanged; E6 poison persists. The healthy S1-S3 rows live in the
    // original view case.
    // | S4a | C1 + either C2 + visit                  | E1+E4+E5+E6 |
    // | S4b | C1 + either C2 + named action           | E2+E5+E6 |
    // | S4c | C1 + either C2 + read/modify            | E3+E4+E5+E6 |
    // | S4d | C1 + either C2 + update/create          | E3+E4+E5+E6 |
    // This exercises the canonical Slot mutex, not MCP's active-marker lock.
    // Only this test inspects a poisoned guard with into_inner; it never clears
    // poison. Production neither recovers nor reads/writes poisoned state.
    use crate::session_realizations::SessionRealizationSource;
    use crate::staged_resources::{CompiledEffectiveInputs, ResourceBindingCheck, StagedResources};
    use awaken_provisioning_contract as pc;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let host = crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .expect("validated Host dependencies");
    let context = host
        .run_assembly
        .ctx_for("present", None)
        .await
        .expect("actual Run context");
    crate::host::tests::bind_test_memory(&host, "present", "poison-memory", true);
    let slots = &host.run_assembly.installed_sessions;
    let memory = slots
        .read("present", |slot| slot.realization.staging().memory.clone())
        .flatten()
        .unwrap();
    let input = crate::session_resource::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: "poison-file".into(),
        source: crate::session_resource::ResolvedInputSource::File {
            file_id: "file-original".into(),
        },
        mount_path: "/original.txt".into(),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: Some("original instructions".into()),
    };
    let skill = awaken_resource_contract::SkillVersion {
        id: "original-version".into(),
        skill_id: "original-skill".into(),
        version: 3,
        name: "original Skill".into(),
        description: "original description".into(),
        directory: "/skills/original".into(),
        bundle_sha256: "original-hash".into(),
        files: Vec::new(),
        created_unix_nanos: 1,
    };
    let claim = awaken_dispatch_contract::RunClaim {
        run_id: awaken_agent_contract::agent::run::Id("original-run".into()),
        owner: "original-worker".into(),
        epoch: 3,
    };
    let resources = StagedResources {
        mounts: vec![pc::MountRequirement {
            mount_id: "original-mount".into(),
            source: pc::MountSource::InlineBytes {
                contents: b"original".to_vec(),
                content_hash: None,
            },
            mount_path: "/original.txt".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
        prompts: vec!["original prompt".into()],
        resource_prompts: vec![
            crate::managed_input_layout::ResourcePromptProjection::try_from_input(&input).unwrap(),
        ],
        binding_checks: vec![ResourceBindingCheck::MemoryStore {
            memory_store_id: "poison-memory".into(),
            config_version: awaken_resource_contract::ConfigVersion(3),
        }],
        repositories: vec![crate::staged_resources::RepositoryActivation {
            plan: pc::RepositoryRealizationPlan {
                repository_id: "original-repository".into(),
                mount_path: "/workspace/repository".into(),
                source_remote_url: "https://example.invalid/original.git".into(),
                transport_url: "https://example.invalid/original.git".into(),
                initial_branch: Some("original-branch".into()),
                initial_commit: Some("original-commit".into()),
                access: pc::MountAccess::ReadWrite,
            },
            credential_pin: None,
        }],
    };
    slots.update("present", |slot| {
        slot.replace_frozen_with(|next| next.workspace = Some("original-workspace".into()));
        slot.realization.staging_mut().resources = resources.clone();
        slot.realization
            .staging_mut()
            .memory_bindings
            .insert("original-binding".into(), memory.clone());
        slot.realization.staging_mut().skills = Some(vec![skill.clone()]);
        slot.realization.staging_mut().effect_key = Some("original-key".into());
        slot.run.dispatch_claim = Some(claim.clone());
        slot.run.session_dispatch = true;
    });
    let frozen = slots.read("present", |slot| slot.inputs.clone()).unwrap();
    let phase_locks = slots
        .read("present", |slot| {
            (
                slot.realization.execution().phase.clone(),
                slot.realization.environment().lifecycle.clone(),
                slot.realization.staging().projection.clone(),
            )
        })
        .unwrap();
    let mut poisoning_callbacks = 0;
    assert!(
        catch_unwind(AssertUnwindSafe(|| slots.modify("present", |_| {
            poisoning_callbacks += 1;
            panic!("poison the actual Slot guard");
        })))
        .is_err()
    );
    assert_eq!(
        poisoning_callbacks, 1,
        "R12 callback panic once under actual guard"
    );
    assert!(slots.slots.is_poisoned());
    let mut callbacks = 0;
    let expected_panic = format!(
        "session runtime slots mutex poisoned: {:?}",
        PoisonError::new(())
    );
    for target in ["present", "absent"] {
        // Q8 actual global Slot poison precedes Primary for either adapter;
        // the delegated future must remain UNPOLLED, for present and absent ids.
        // Preserve each original prefix/Debug, not a common substituted panic.
        for legacy in [false, true] {
            use futures_util::FutureExt;
            let polls = std::sync::atomic::AtomicUsize::new(0);
            let refused = AssertUnwindSafe(
                crate::session_realizations::primary_or_delegated_run_active(
                    target,
                    |id| {
                        if legacy {
                            Ok(slots
                                .read_run(id, |run| {
                                    run.context
                                        .as_ref()
                                        .is_some_and(|runtime| runtime.has_active_run())
                                })
                                .unwrap_or(false))
                        } else {
                            Ok(host
                                .realization_executor
                                .sessions()
                                .modify_session(id, |slot| slot.run.has_active_run())?
                                .unwrap_or(false))
                        }
                    },
                    async {
                        polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok::<_, crate::HostError>(Vec::new())
                    },
                ),
            )
            .catch_unwind()
            .await;
            if legacy {
                let message = refused.expect_err("Q8 legacy Slot poison panics");
                let message = message
                    .downcast_ref::<String>()
                    .expect("Q8 original expect panic");
                assert_eq!(message, &expected_panic, "Q8 legacy std Debug unchanged");
            } else {
                let denied = refused
                    .expect("Q8 the view denies instead of panicking")
                    .expect_err("Q8 actual Slot poison must deny");
                assert_eq!(
                    denied.code,
                    crate::realization_error::POISONED_CODE,
                    "Q8 typed view denial"
                );
            }
            assert_eq!(
                polls.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "Q8 delegated read never polled"
            );
        }
        let refused = slots.visit(target, &mut |_| callbacks += 1);
        assert!(
            matches!(
                refused,
                Err(crate::realization_error::RealizationLock::InstalledSessions)
            ),
            "S4a {target}"
        );
        let publication = catch_unwind(AssertUnwindSafe(|| {
            slots.publish_resource_requirements(
                target,
                "forbidden-workspace",
                None,
                CompiledEffectiveInputs {
                    staged: StagedResources {
                        prompts: vec!["forbidden replacement".into()],
                        ..Default::default()
                    },
                    memory_bindings: Default::default(),
                },
                Vec::new(),
                "forbidden-key".into(),
            )
        }))
        .expect("S4b named Source action returns rather than panics")
        .unwrap_err();
        assert!(
            matches!(
                publication,
                crate::realization_error::RealizationError::Poisoned(
                    crate::realization_error::RealizationLock::InstalledSessions
                )
            ),
            "S4b {target}"
        );
        let panics = [
            catch_unwind(AssertUnwindSafe(|| slots.read(target, |_| callbacks += 1))).unwrap_err(),
            catch_unwind(AssertUnwindSafe(|| {
                slots.modify(target, |_| callbacks += 1)
            }))
            .unwrap_err(),
            catch_unwind(AssertUnwindSafe(|| {
                slots.update(target, |_| callbacks += 1)
            }))
            .unwrap_err(),
        ];
        for panic in panics {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .expect("original panic string");
            assert_eq!(
                message, expected_panic,
                "S4c/S4d {target} actual std Debug retained"
            );
        }
    }
    assert_eq!(callbacks, 0, "S4a/S4c/S4d no callback entered");
    assert!(slots.slots.is_poisoned(), "S4/E6 poison remains");
    let poisoned_guard = match slots.slots.lock() {
        Err(error) => error.into_inner(),
        Ok(_) => panic!("S4/E6 no recovery or clear_poison"),
    };
    assert_eq!(poisoned_guard.len(), 1, "S4/E5 no target created");
    assert!(
        !poisoned_guard.contains_key("absent"),
        "S4 absent remains absent"
    );
    let slot = &poisoned_guard["present"];
    assert!(Arc::ptr_eq(&slot.inputs, &frozen), "S4/E5 whole Frozen Arc");
    assert_eq!(slot.inputs.workspace.as_deref(), Some("original-workspace"));
    assert_eq!(
        slot.realization.staging().resources.mounts,
        resources.mounts
    );
    assert_eq!(
        slot.realization.staging().resources.prompts,
        resources.prompts
    );
    assert_eq!(
        format!(
            "{:?}",
            slot.realization.staging().resources.resource_prompts
        ),
        format!("{:?}", resources.resource_prompts)
    );
    assert_eq!(slot.realization.staging().resources.binding_checks.len(), 1);
    assert!(
        matches!(&slot.realization.staging().resources.binding_checks[0], ResourceBindingCheck::MemoryStore {
        memory_store_id, config_version,
    } if memory_store_id == "poison-memory" && config_version.0 == 3)
    );
    assert!(slot.realization.staging().resources.repositories == resources.repositories);
    assert_eq!(slot.realization.staging().memory_bindings.len(), 1);
    assert!(Arc::ptr_eq(
        &slot.realization.staging().memory_bindings["original-binding"],
        &memory
    ));
    assert!(Arc::ptr_eq(
        slot.realization.staging().memory.as_ref().unwrap(),
        &memory
    ));
    assert_eq!(slot.realization.staging().skills, Some(vec![skill]));
    assert_eq!(
        slot.realization.staging().effect_key.as_deref(),
        Some("original-key")
    );
    assert!(
        Arc::ptr_eq(slot.run.context.as_ref().unwrap(), &context),
        "S4/E5 Run context"
    );
    assert_eq!(slot.run.dispatch_claim, Some(claim));
    assert!(slot.run.session_dispatch);
    assert!(Arc::ptr_eq(
        &slot.realization.execution().phase,
        &phase_locks.0
    ));
    assert!(Arc::ptr_eq(
        &slot.realization.environment().lifecycle,
        &phase_locks.1
    ));
    assert!(Arc::ptr_eq(
        &slot.realization.staging().projection,
        &phase_locks.2
    ));
}

#[tokio::test]
async fn local_activity_combination_keeps_real_poison_fault_order() {
    // Necessary Q9-Q11 fault partitions extend the original S4 setup: create
    // fresh actual Hosts because neither global Slot nor active-marker poison
    // may be cleared/recovered. No fake Source/view, registry or bool adapter.
    // Q9 two adapters + Primary active + awaiting reader poisons Slot + Open
    // child -> the legacy adapter's original panic, the view's typed Poisoned
    // denial (true Primary never skips either).
    // Q10 two adapters + Primary inactive + same awaited poison + Completed
    // child -> false without a child query. Q11 root marker poison precedes
    // reader; occupied child marker poison follows reader even if Primary true;
    // Completed child marker poison is never queried. All actual writers join.
    // Fresh Hosts share the process-default Sandbox root: each independent row
    // needs its own stable Session identity, rather than reclaiming another
    // row's still-owned directory. Queries within a row retain that identity.
    use crate::session_realizations::primary_or_delegated_run_active;
    use awaken_agent_contract::agent::delegation::DelegationStatus as Status;
    use futures_util::FutureExt;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let relationship = |child: &str, status| awaken_session_application_contract::DelegatedRun {
        run_id: awaken_agent_contract::agent::run::Id(child.into()),
        parent_call_id: "query-call".into(),
        agent_id: "query-agent".into(),
        status,
    };
    for legacy in [false, true] {
        for (primary_active, status) in [(true, Status::Open), (false, Status::Completed)] {
            let root_id = format!("query-slot-root-{legacy}-{primary_active}-{status:?}");
            let child_id = format!("query-slot-child-{legacy}-{primary_active}-{status:?}");
            let host = crate::HostDependencies::ephemeral(
                Arc::new(crate::no_model::NoModelConfiguredExecutor),
                "stub",
            )
            .await
            .build()
            .expect("Q9/Q10 actual Host");
            let ctx = host
                .run_assembly
                .ctx_for(&root_id, None)
                .await
                .expect("Q9 actual context");
            host.run_assembly
                .installed_sessions
                .update_run(&root_id, |run| run.context = Some(ctx.clone()));
            ctx.foreground.install_for_test(
                primary_active
                    .then(|| awaken_agent_contract::agent::run::Id("active-primary".into())),
            );
            let view = host.run_assembly.installed_sessions.realizations();
            let polls = std::sync::atomic::AtomicUsize::new(0);
            let (enter, entry) = tokio::sync::oneshot::channel::<()>();
            let writer = tokio::spawn({
                let slots = host.run_assembly.installed_sessions.clone();
                let root_id = root_id.clone();
                async move {
                    if entry.await.is_err() {
                        return false;
                    }
                    catch_unwind(AssertUnwindSafe(|| {
                        slots.modify(&root_id, |_| {
                            panic!("Q9/Q10 actual Slot poison during reader")
                        });
                    }))
                    .is_err()
                }
            });
            // The reader itself joins its actual writer before returning rows.
            // If a regressed helper skips the future, its sender drops and the
            // writer exits; keep its handle outside so every path is joined.
            let mut writer = writer;
            let observed = AssertUnwindSafe(primary_or_delegated_run_active(
                &root_id,
                |id| {
                    if legacy {
                        Ok(host
                            .run_assembly
                            .installed_sessions
                            .read_run(id, |run| {
                                run.context
                                    .as_ref()
                                    .is_some_and(|runtime| runtime.has_active_run())
                            })
                            .unwrap_or(false))
                    } else {
                        Ok(view
                            .modify_session(id, |slot| slot.run.has_active_run())?
                            .unwrap_or(false))
                    }
                },
                async {
                    polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    enter.send(()).expect("Q9 actual delegation reader entry");
                    assert!(
                        (&mut writer).await.expect("Q9 writer joined within reader"),
                        "Q9 actual caught callback poisoned Slot"
                    );
                    Ok::<_, crate::HostError>(vec![relationship(&child_id, status)])
                },
            ))
            .catch_unwind()
            .await;
            // JoinHandle returns after completion; do not poll the same handle
            // twice. The poll count distinguishes the skipped-reader path.
            if polls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                let _ = writer.await;
            }
            assert_eq!(
                polls.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "Q9/Q10 true Primary still polls reader"
            );
            assert!(
                host.run_assembly.installed_sessions.slots.is_poisoned(),
                "Q9/Q10 actual Slot poison"
            );
            if primary_active {
                if legacy {
                    let panic =
                        observed.expect_err("Q9 active Primary must not suppress child poison");
                    let message = panic
                        .downcast_ref::<String>()
                        .expect("Q9 original expect panic");
                    assert_eq!(
                        message,
                        &format!(
                            "session runtime slots mutex poisoned: {:?}",
                            PoisonError::new(())
                        ),
                        "Q9 legacy diagnostic"
                    );
                } else {
                    let denied = observed
                        .expect("Q9 the view denies instead of unwinding")
                        .expect_err("Q9 active Primary must not suppress child poison");
                    assert_eq!(
                        denied.code,
                        crate::realization_error::POISONED_CODE,
                        "Q9 view denial"
                    );
                }
            } else {
                assert!(
                    !observed
                        .expect("Q10 Completed skips poisoned child Slot")
                        .unwrap(),
                    "Q10 local inactive result only"
                );
            }
        }
        for (primary_fault, status) in [
            (true, Status::Open),
            (false, Status::Open),
            (false, Status::Completed),
        ] {
            let root_id = format!("query-marker-root-{legacy}-{primary_fault}-{status:?}");
            let child_id = format!("query-marker-child-{legacy}-{primary_fault}-{status:?}");
            let host = crate::HostDependencies::ephemeral(
                Arc::new(crate::no_model::NoModelConfiguredExecutor),
                "stub",
            )
            .await
            .build()
            .expect("Q11 actual Host");
            let root = host
                .run_assembly
                .ctx_for(&root_id, None)
                .await
                .expect("Q11 actual root");
            let child = host
                .run_assembly
                .ctx_for(&child_id, None)
                .await
                .expect("Q11 actual child");
            host.run_assembly
                .installed_sessions
                .update_run(&root_id, |run| run.context = Some(root.clone()));
            host.run_assembly
                .installed_sessions
                .update_run(&child_id, |run| run.context = Some(child.clone()));
            root.foreground.install_for_test(
                (!primary_fault && status == Status::Open)
                    .then(|| awaken_agent_contract::agent::run::Id("active-primary".into())),
            );
            let fault = if primary_fault { root } else { child };
            let marker_writer = std::thread::spawn(move || {
                let _held = fault.foreground.hold_for_test();
                panic!("Q11 actual active-marker poison");
            });
            assert!(marker_writer.join().is_err(), "Q11 marker writer joined");
            assert!(
                !host.run_assembly.installed_sessions.slots.is_poisoned(),
                "Q11 distinct healthy Slot before query"
            );
            let view = host.run_assembly.installed_sessions.realizations();
            let polls = std::sync::atomic::AtomicUsize::new(0);
            let observed = AssertUnwindSafe(primary_or_delegated_run_active(
                &root_id,
                |id| {
                    if legacy {
                        Ok(host
                            .run_assembly
                            .installed_sessions
                            .read_run(id, |run| {
                                run.context
                                    .as_ref()
                                    .is_some_and(|runtime| runtime.has_active_run())
                            })
                            .unwrap_or(false))
                    } else {
                        Ok(view
                            .modify_session(id, |slot| slot.run.has_active_run())
                            .expect("installed Sessions")
                            .unwrap_or(false))
                    }
                },
                async {
                    polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok::<_, crate::HostError>(vec![relationship(&child_id, status)])
                },
            ))
            .catch_unwind()
            .await;
            // A poisoned foreground record is a typed refusal that counts as
            // active, never a panic: the reader is still polled after Primary.
            assert_eq!(
                polls.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "Q11 Primary first, then the reader"
            );
            let observed = observed.expect("Q11 marker poison never panics");
            if status == Status::Completed {
                assert!(!observed.unwrap(), "Q11 no context-presence substitution");
            } else {
                assert!(
                    observed.unwrap(),
                    "Q11 actual active-marker poison cannot become inactive"
                );
            }
        }
    }
}
