//! Background quiescence keys and terminal reconstruction/disposal replay.

use super::*;
use awaken_session_configuration_contract::resource as session_resource;

#[tokio::test]
async fn durable_activity_generation_is_the_only_background_quiescence_key() {
    // Cause/effect table for the background-task/C2-b proof-sync overlap: C1 an
    // exact durable Resident owns one Arc+handle; C2 its physical sandbox id is
    // different from the aggregate Sandbox generation id; C3 SharedEnvironment
    // work is active; C4 that work releases. Effects: E1 the owner projects only
    // the durable id; E2 querying the physical id observes no false ownership;
    // E3 durable-id quiescence cannot prove before C4; E4 it proves after C4.
    // LegacyDirect retains the physical-id branch in the same owner method, and
    // an Environment-free context retains the existing `brain` key.
    let thread = "durable-background-generation";
    let root = tempfile::tempdir().unwrap();
    let provider = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let physical_id = environment.lifecycle().handle().sandbox_id;
    let durable_generation = generation(thread);
    assert_ne!(physical_id, durable_generation.id(), "C2");
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    host.installed_sessions
        .realizations()
        .update_realization(thread, |r| {
            r.environment.owner = SessionEnvironmentOwner::Resident(BoundSessionEnvironment {
                identity: BoundSessionEnvironmentIdentity::Durable {
                    effect_id: format!("effect-{thread}"),
                    generation: durable_generation.clone(),
                },
                binding: serde_json::to_string(&environment.lifecycle().handle()).unwrap(),
                environment: environment.clone(),
            });
        });
    let (activity_generation, origin) = host
        .realizer
        .resident_environment_activity_identity(thread, &environment)
        .expect("C1 exact Resident activity projection");
    assert_eq!(activity_generation, durable_generation.id(), "E1");
    assert_eq!(
        origin.as_ref().unwrap().as_str(),
        durable_generation.id(),
        "E1 original Bound resource origin, not the mutable effect id"
    );

    let release = Arc::new(tokio::sync::Notify::new());
    let background = host.resources.memory.background();
    background
        .spawn(
            crate::background::BackgroundWorkClass::SharedEnvironment {
                session_id: thread.into(),
                generation_id: activity_generation.clone(),
            },
            {
                let release = release.clone();
                async move {
                    tokio::time::timeout(std::time::Duration::from_secs(30), release.notified())
                        .await
                        .expect("timed out waiting for release.notified()")
                }
            },
        )
        .await;
    assert!(
        background
            .quiesce_shared_environment(thread, &physical_id, std::time::Duration::from_millis(1),)
            .await,
        "E2 physical id is not the durable activity owner"
    );
    assert!(
        !background
            .quiesce_shared_environment(
                thread,
                &activity_generation,
                std::time::Duration::from_millis(1),
            )
            .await,
        "E3 no quiescence proof while exact durable activity remains"
    );
    release.notify_one();
    assert!(
        background
            .quiesce_shared_environment(
                thread,
                &activity_generation,
                std::time::Duration::from_secs(1),
            )
            .await,
        "E4 exact durable activity release admits quiescence"
    );
    // O1 the reference is read from the actual Bound generation even if the
    // mutable effect changes. O2 a foreign Arc fails instead of borrowing its
    // desired key. O3 legacy activity fallback remains usable but cannot
    // manufacture the new provenance reference or historical Create evidence.
    host.installed_sessions
        .realizations()
        .update_realization(thread, |r| {
            let SessionEnvironmentOwner::Resident(owned) = &mut r.environment.owner else {
                panic!("O1 Resident")
            };
            let BoundSessionEnvironmentIdentity::Durable { effect_id, .. } = &mut owned.identity
            else {
                panic!("O1 durable")
            };
            *effect_id = "later-adopt-effect".into();
        });
    let (_, after_adopt) = host
        .realizer
        .resident_environment_activity_identity(thread, &environment)
        .unwrap();
    assert_eq!(after_adopt, origin, "O1 original generation survives Adopt");
    let foreign = super::environment(&provider, "foreign-origin").await;
    assert!(
        host.realizer
            .resident_environment_activity_identity(thread, &foreign)
            .is_err(),
        "O2 exact Resident Arc required"
    );
    host.installed_sessions
        .realizations()
        .update_realization(thread, |r| {
            let SessionEnvironmentOwner::Resident(owned) = &mut r.environment.owner else {
                panic!("O3 Resident")
            };
            owned.identity = BoundSessionEnvironmentIdentity::LegacyDirect(
                crate::environment_owner::LegacyDirectEnvironmentProvenance::DurableBinding {
                    binding: owned.binding.clone(),
                    effect: crate::environment_owner::LegacyEnvironmentEffect::Absent,
                },
            );
        });
    let (legacy_activity, unknown_origin) = host
        .realizer
        .resident_environment_activity_identity(thread, &environment)
        .unwrap();
    assert_eq!(legacy_activity, physical_id, "O3 old activity key");
    assert!(unknown_origin.is_none(), "O3 no generation/source backfill");
}

#[tokio::test(start_paused = true)]
async fn terminal_background_fence_preserves_resident_and_retiring_outputs_for_retry() {
    use crate::host::worker_resolver::test_support::{
        eager_environment, empty_frozen_projection_for_snapshot, test_activation,
    };
    use awaken_provisioning_contract::SandboxStatus;

    // Cause/effect table: C1 the sole owner is either Resident or a retryable
    // Retiring::Bound; C2 identity is Durable or LegacyDirect and therefore
    // selects the durable generation or physical sandbox key respectively; C3
    // exact SharedEnvironment work remains active beyond the bounded wait; C4
    // that work releases and the identical aggregate-authorized Preparation
    // retries; C5 an Agent-authored Skill remains run-scoped without an explicit
    // PromotionGate; C6 the aggregate durably records that receipt and projects the
    // separate physical Disposal. Effects: E1 C3 returns classified Unavailable
    // with zero Skill/Artifact harvest and zero physical disposal while retaining
    // the exact Arc under the terminal retirement fence; E2 the other identity
    // key cannot substitute; E3 C4 returns one Artifact Preparation receipt,
    // leaves C5 unpublished, and leaves the physical source Ready; E4 C6 performs
    // one disposal and only the subsequent
    // durable acknowledgement removes the owner. Rules T1-T4 cross
    // Resident/Retiring with Durable/Legacy under C3=>E1+E2, C4=>E3, C5=>E4.
    for (thread, retiring, durable) in [
        ("terminal-background-resident-durable", false, true),
        ("terminal-background-retiring-durable", true, true),
        ("terminal-background-resident-legacy", false, false),
        ("terminal-background-retiring-legacy", true, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let host = (crate::HostDependencies::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await
        .with_store_dir(root.path())
        .await
        .with_skill_store(root.path().join("skill-store")))
        .build()
        .expect("validated Host dependencies");
        let activation = test_activation(thread, &format!("terminal-background-{thread}"));
        let mut projection = empty_frozen_projection_for_snapshot(
            "workspace",
            eager_environment(),
            &activation.snapshot,
        );
        host.install_dispatch_frozen_session_projection(thread, projection.clone())
            .await
            .expect("C1 install complete physical projection before creation");
        let lease = session_resource::SessionRealizationLease {
            owner: format!("terminal-background-worker-{thread}"),
            runtime_incarnation: format!("terminal-background-worker-{thread}:incarnation"),
            epoch: 1,
            expires_at_unix_ms: awaken_session_realization::test_support::runtime_unix_now_ms()
                + 60_000,
        };
        let create_fence = lease
            .sandbox_effect_fence(format!("terminal-background-create-{thread}"))
            .expect("C1 create exact durable Sandbox fence");
        let spec = host.realizer.sandbox_spec(thread);
        let physical = host
            .test_local_provider()
            .create_sandbox_for_effect(
                &spec,
                &create_fence,
                None,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&create_fence),
            )
            .await
            .expect("C1 create provider-effective terminal Sandbox");
        let environment = Arc::new(
            awaken_session_environment::SandboxEnvironment::workdir(physical)
                .into_session_environment(),
        );
        let physical_id = environment.lifecycle().handle().sandbox_id;
        let durable_generation = generation(thread);
        assert_ne!(physical_id, durable_generation.id(), "C2");
        let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
        let identity = if durable {
            BoundSessionEnvironmentIdentity::Durable {
                effect_id: create_fence.operation_id().to_owned(),
                generation: durable_generation.clone(),
            }
        } else {
            BoundSessionEnvironmentIdentity::LegacyDirect(
                LegacyDirectEnvironmentProvenance::DurableBinding {
                    binding: binding.clone(),
                    effect: crate::environment_owner::LegacyEnvironmentEffect::Absent,
                },
            )
        };
        let (activity_generation_id, other_generation_id) = if durable {
            (durable_generation.id().to_owned(), physical_id.clone())
        } else {
            (physical_id.clone(), durable_generation.id().to_owned())
        };
        let owned = BoundSessionEnvironment {
            identity,
            binding: binding.clone(),
            environment: environment.clone(),
        };
        // A changed aggregate Environment is a new root-CAS snapshot.
        projection.revision.0 += 1;
        projection.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: binding.clone(),
            effect_id: durable.then(|| create_fence.operation_id().to_owned()),
            generation: durable.then(|| durable_generation.clone()),
            idle_since_unix_ms: None,
        };
        host.installed_sessions
            .realizations()
            .update_realization(thread, |r| {
                r.environment.owner = SessionEnvironmentOwner::Resident(owned.clone());
            });

        let output = root
            .path()
            .join("sandboxes")
            .join(thread)
            .join(spec.outputs_path.trim_start_matches('/'))
            .join("report.txt");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, format!("report-{thread}")).unwrap();
        let skill_dir = root
            .path()
            .join("sandboxes")
            .join(thread)
            .join("skills")
            .join("notes");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: authored before terminal retry\n---\nretain me",
        )
        .unwrap();
        assert!(
            environment
                .skills()
                .scan_skill_dir(crate::skills::DEFAULT_SKILLS_SUBDIR)
                .unwrap()
                .iter()
                .any(|skill| skill.id == "notes"),
            "C5 physical authored Skill exists before cleanup"
        );
        let release = Arc::new(tokio::sync::Notify::new());
        let background = host.resources.memory.background();
        background
            .spawn(
                crate::background::BackgroundWorkClass::SharedEnvironment {
                    session_id: thread.into(),
                    generation_id: activity_generation_id.clone(),
                },
                {
                    let release = release.clone();
                    async move { release.notified().await }
                },
            )
            .await;
        if retiring {
            let realization = host
                .installed_sessions
                .realizations()
                .realization_lock(thread);
            let _realization = realization.lock().await;
            let lifecycle = host
                .installed_sessions
                .realizations()
                .read_realization(thread, |r| r.environment.lifecycle.clone())
                .unwrap();
            let _lifecycle = lifecycle.lock().await;
            assert!(
                host.realizer
                    .retire_session_environment_for_revocation(
                        thread,
                        (&host.mcp, host.installed_sessions.as_ref())
                    )
                    .await
                    .unwrap(),
                "C1 public revocation transition"
            );
            assert_eq!(
                host.registered_thread_workspace(thread).as_deref(),
                Some("workspace"),
                "C1 Retiring owner keeps exact terminal harvest scope"
            );
        }
        // Real takeover ordering: revocation first clears the old realization
        // generation while retaining the exact physical owner; only then does
        // Control's terminal assignment install its new cleanup lease.
        host.install_test_terminal_cleanup_projection(
            &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
                environment_creation_not_attempted: false,
                session_id: thread.into(),
                projection: projection.clone(),
                authority: awaken_session_realization::test_support::test_lease_receipt(
                    lease.clone(),
                ),
            },
        )
        .await
        .expect("C1 install exact terminal projection after any revocation");

        let mut aggregate = awaken_session_state_contract::PersistedSession::frozen_with_budget(
            thread,
            projection.baseline.clone(),
            awaken_session_state_contract::SessionResourceState::from_active(
                projection.resources.clone(),
            ),
            Default::default(),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        aggregate.environment = projection.environment.clone();
        aggregate.realization = Some(lease.clone());
        assert!(aggregate.ensure_terminal_cleanup_fence());
        aggregate
            .freeze_terminal_cleanup_targets(std::iter::empty(), 0, 0)
            .expect("C3 freeze exact root terminal target");
        let command = aggregate
            .terminal_cleanup
            .command_for(&awaken_session_types::SessionId::from(thread), thread)
            .expect("C3 aggregate terminal command");
        let effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
            command,
            lease.clone(),
        );
        let inherited_provider_disposal = aggregate
            .authorize_terminal_cleanup_effect(&effect)
            .expect("C3 aggregate preparation authorization");
        let authorization =
            awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
                effect.clone(),
                projection.workspace_id.clone(),
                inherited_provider_disposal,
            )
            .expect("C3 closed terminal preparation authorization")
            .with_source_dispatch(awaken_session_realization_contract::SessionTerminalSourceDispatch::Settled {});
        // Source-admission rules S1 Pending/S2 Unavailable + a Ready root or
        // child live-Artifact path -> refuse before background, Artifact,
        // Memory or provider data I/O. A child's absent root_environment value
        // is not a provider-validated receipt-only source.
        // The existing E1/E2 retries below retain their explicit Settled premise
        // and still prove the independent shared-background fence.
        let child = format!("{thread}-unsettled-child");
        let mut child_aggregate = aggregate.clone();
        child_aggregate.terminal_cleanup = Default::default();
        assert!(child_aggregate.ensure_terminal_cleanup_fence());
        child_aggregate
            .freeze_terminal_cleanup_targets([child.clone()], 0, 0)
            .unwrap();
        let child_effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
            child_aggregate
                .terminal_cleanup
                .command_for(&thread.into(), &child)
                .unwrap(),
            lease.clone(),
        );
        let child_authorization = awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
            child_effect.clone(),
            projection.workspace_id.clone(),
            child_aggregate.authorize_terminal_cleanup_effect(&child_effect).unwrap(),
        ).unwrap();
        for (guarded_effect, guarded_authorization) in [
            (effect.clone(), authorization.clone()),
            (child_effect, child_authorization),
        ] {
            for source in [
                awaken_session_realization_contract::SessionTerminalSourceDispatch::Pending {},
                awaken_session_realization_contract::SessionTerminalSourceDispatch::Unavailable {
                    detail: "fixture source observation outage".into(),
                },
            ] {
                let error = awaken_session_execution_contract::TerminalCleanupRuntime::prepare_terminal_cleanup_for_effect(
                    &crate::ManagedHost::new(host.clone()),
                    guarded_effect.clone(),
                    guarded_authorization.clone().with_source_dispatch(source),
                ).await.map_err(crate::host_error::from_run_error)
                    .unwrap_err();
                assert_eq!(
                    error.code, "session_terminal_source_dispatch_not_settled",
                    "S1/S2 {error:?}"
                );
                assert!(
                    host.file_application()
                        .unwrap()
                        .list("workspace", Some(thread))
                        .await
                        .unwrap()
                        .is_empty(),
                    "S1/S2 no live harvest"
                );
            }
        }
        let blocked = tokio::spawn({
            let host = host.clone();
            let effect = effect.clone();
            let authorization = authorization.clone();
            async move {
                awaken_session_execution_contract::TerminalCleanupRuntime::prepare_terminal_cleanup_for_effect(
                    &crate::ManagedHost::new(host.clone()),
                    effect,
                    authorization,
                ).await.map_err(crate::host_error::from_run_error)
            }
        });
        // A paused Tokio runtime may auto-advance once the quiescence timeout
        // is the only runnable future, so the classified timeout itself is the
        // stable evidence; observing an intermediate scheduler state is not.
        let error = blocked.await.unwrap().unwrap_err();
        assert_eq!(error.kind, HostErrorKind::Unavailable, "E1: {error:?}");
        assert_eq!(
            error.code, "session_terminal_background_not_quiescent",
            "E1"
        );
        assert!(
            host.file_application()
                .unwrap()
                .list("workspace", Some(thread))
                .await
                .unwrap()
                .is_empty(),
            "E1 zero harvest"
        );
        assert!(
            host.tools
                .skills
                .definitions("workspace")
                .await
                .unwrap()
                .is_empty(),
            "E1 zero Skill harvest"
        );
        assert_eq!(
            environment.sandbox_environment().status().await.unwrap(),
            SandboxStatus::Ready,
            "E1"
        );
        let retained = host
            .installed_sessions
            .realizations()
            .read_realization(thread, |r| r.environment.owner.clone())
            .unwrap();
        assert!(
            matches!(
                retained,
                SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                    cause: SessionEnvironmentRetirementCause::Terminal { ref effect_id },
                    owned: RetiringEnvironmentOwner::Bound(ref current),
                }) if effect_id == effect.operation_id() && current.exact_matches(&owned)
            ),
            "E1 exact Arc retained under the canonical terminal fence"
        );
        assert!(
            background.has_shared_environment_work(thread, &activity_generation_id),
            "E2 exact owner-selected activity remains visible"
        );
        assert!(
            !background.has_shared_environment_work(thread, &other_generation_id),
            "E2 the other identity domain is not a substitute activity key"
        );

        release.notify_one();
        let preparation = awaken_session_execution_contract::TerminalCleanupRuntime::prepare_terminal_cleanup_for_effect(
                    &crate::ManagedHost::new(host.clone()),
                    effect.clone(),
                    authorization,
                ).await.map_err(crate::host_error::from_run_error)
            .expect("C4 identical Preparation retry");
        assert_eq!(
            preparation.artifact_receipts.len(),
            1,
            "E3 one Preparation harvest receipt"
        );
        assert_eq!(
            host.file_application()
                .unwrap()
                .list("workspace", Some(thread))
                .await
                .unwrap()
                .len(),
            1,
            "E3 one durable Artifact"
        );
        assert!(
            host.tools
                .skills
                .definitions("workspace")
                .await
                .unwrap()
                .is_empty(),
            "E3 cleanup cannot self-promote an Agent-authored Skill; explicit publication remains the canonical owner"
        );
        assert_eq!(
            environment.sandbox_environment().status().await.unwrap(),
            SandboxStatus::Ready,
            "E3 Preparation is non-destructive"
        );

        let repository_preparation =
            awaken_session_state_contract::SessionCleanupRepositoryPreparation::new(
                &awaken_session_types::SessionId::from(thread),
                &projection.workspace_id,
                &aggregate.resources,
            )
            .expect("C5 prepare aggregate Repository retirement");
        aggregate
            .record_terminal_cleanup_preparation(
                &projection.workspace_id,
                &lease,
                preparation,
                Some(repository_preparation),
            )
            .expect("C5 durably admit exact Preparation");
        host.realizer
            .cleanup
            .acknowledge_terminal_cleanup_preparation(&host.mcp, &effect)
            .await;
        let disposal_command = match aggregate
            .terminal_cleanup_work_action()
            .expect("C5 project canonical terminal action")
            .expect("C5 complete Preparation projects Disposal")
        {
            awaken_session_state_contract::SessionTerminalCleanupAction::Dispose { command } => {
                command
            }
            action => panic!("C5 expected Disposal, got {action:?}"),
        };
        let disposal_effect =
            awaken_session_state_contract::SessionTerminalCleanupDisposalEffect::new(
                disposal_command,
                lease.clone(),
            );
        let disposal_receipt = host
            .realizer
            .cleanup
            .dispose_terminal_cleanup_effect(&host.realizer, disposal_effect.clone())
            .await
            .expect("C5 aggregate-authorized physical Disposal");
        assert_eq!(
            environment.sandbox_environment().status().await.unwrap(),
            SandboxStatus::Terminated,
            "E4 one physical disposal"
        );
        assert!(
            host.installed_sessions.contains(thread),
            "E4 waits for durable ack"
        );
        aggregate
            .record_terminal_cleanup_disposal(
                &projection.workspace_id,
                &lease,
                disposal_receipt,
                "terminal background test cleanup",
            )
            .expect("C5 durably admit exact Disposal receipt");
        host.realizer
            .cleanup
            .acknowledge_terminal_cleanup_disposal(&host.mcp, &disposal_effect)
            .await;
        assert!(
            !host.installed_sessions.contains(thread),
            "E4 owner removed after ack"
        );
    }
}

#[tokio::test]
async fn terminal_unmaterialized_reconstruction_replays_until_durable_disposal_ack() {
    use crate::host::worker_resolver::test_support::{
        eager_environment, empty_frozen_projection_for_snapshot, test_activation,
    };
    // Causes: C1 Unmaterialized aggregate with a provider-proved Ready root but
    // no published binding; C2 Preparation response loss; C3 durable Preparation
    // admission; C4 Disposal response loss; C5 durable Disposal acknowledgement.
    // Effects: U1 C1 reconstructs one hidden Unbound terminal owner, without
    // Artifact harvest or physical deletion; U2 C2 reuses the exact Arc/handle;
    // U3 C3 authorizes deletion without inventing a durable Environment binding;
    // U4 C4 retries with that same retained owner; U5 only C5 removes the slot.
    // The real Local provider owns marker identity and byte deletion. The real
    // aggregate owns both receipt gates; an empty dispatch queue is no proof.
    let thread = "terminal-unmaterialized-replay";
    let root = tempfile::tempdir().unwrap();
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .with_store_dir(root.path())
    .await)
        .build()
        .expect("validated Host dependencies");
    let activation = test_activation(thread, "terminal-unmaterialized");
    let mut projection = empty_frozen_projection_for_snapshot(
        "workspace",
        eager_environment(),
        &activation.snapshot,
    );
    assert!(matches!(
        projection.environment,
        awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
    ));
    let lease = session_resource::SessionRealizationLease {
        owner: "terminal-worker".into(),
        runtime_incarnation: "terminal-worker:boot".into(),
        epoch: 1,
        expires_at_unix_ms: awaken_session_realization::test_support::runtime_unix_now_ms()
            + 60_000,
    };
    // U1 requires the fresh root's original admission before physical I/O;
    // historical omission remains the explicit negative row in the cold table.
    let mut aggregate =
        crate::host::tests::fresh_terminal_fixture_before_create(thread, &mut projection, &lease);
    host.install_test_terminal_cleanup_projection(
        &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            environment_creation_not_attempted: false,
            session_id: thread.into(),
            projection: projection.clone(),
            authority: awaken_session_realization::test_support::test_lease_receipt(lease.clone()),
        },
    )
    .await
    .unwrap();
    let intent = awaken_session_state_contract::SessionEnvironmentEffectIntent::new(
        thread,
        awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
        Some(lease.clone()),
    )
    .for_environment(projection.baseline.environment.config_fingerprint.0.clone());
    let fence = lease.sandbox_effect_fence(intent.effect_id()).unwrap();
    let spec = host.realizer.sandbox_spec(thread);
    let physical = host
        .test_local_provider()
        .create_sandbox_for_effect(
            &spec,
            &fence,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&fence),
        )
        .await
        .unwrap();
    // The creation response is not published to the Host or Session root.
    drop(physical);
    let output = root
        .path()
        .join("sandboxes")
        .join(thread)
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("unpublished.txt");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"unpublished source must not become an Artifact").unwrap();
    assert!(aggregate.ensure_terminal_cleanup_fence());
    aggregate
        .freeze_terminal_cleanup_targets(std::iter::empty(), 0, 0)
        .unwrap();
    let effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        aggregate
            .terminal_cleanup
            .command_for(&awaken_session_types::SessionId::from(thread), thread)
            .unwrap(),
        lease.clone(),
    );
    let authorization =
        awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
            effect.clone(),
            projection.workspace_id.clone(),
            aggregate
                .authorize_terminal_cleanup_effect(&effect)
                .unwrap(),
        )
        .unwrap()
        .with_source_dispatch(awaken_session_realization_contract::SessionTerminalSourceDispatch::Settled {});
    let first = awaken_session_execution_contract::TerminalCleanupRuntime::prepare_terminal_cleanup_for_effect(
                    &crate::ManagedHost::new(host.clone()),
                    effect.clone(),
                    authorization.clone(),
                ).await.map_err(crate::host_error::from_run_error)
        .expect("U1 provider-proved reconstruction");
    assert!(
        first.artifact_receipts.is_empty(),
        "U1 no unpublished output harvest"
    );
    assert!(output.exists(), "U1 Preparation cannot delete");
    let retained = host
        .installed_sessions
        .realizations()
        .read_realization(thread, |r| r.environment.owner.clone())
        .unwrap();
    let SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
        owned: RetiringEnvironmentOwner::Unbound(ref candidate),
        ..
    }) = retained
    else {
        panic!("U1 exactly one Unbound terminal owner")
    };
    assert!(
        candidate.durable_identity().is_none(),
        "U1 no fabricated receipt"
    );
    assert!(
        host.realizer.session_environment(thread).await.is_none(),
        "U1 hidden"
    );
    // U6 crosses foreign terminal operation and stale lease with both phases.
    // Each must preserve the physical bytes; the exact valid retry still works.
    let mut rejected_owners = vec![SessionEnvironmentOwner::Retiring(
        RetiringSessionEnvironment {
            owned: RetiringEnvironmentOwner::Unbound(candidate.clone()),
            cause: SessionEnvironmentRetirementCause::Terminal {
                effect_id: "foreign-preparation".into(),
            },
        },
    )];
    for wrong in &rejected_owners {
        host.installed_sessions
            .realizations()
            .update_realization(thread, |r| r.environment.owner = wrong.clone());
        awaken_session_execution_contract::TerminalCleanupRuntime::prepare_terminal_cleanup_for_effect(
                    &crate::ManagedHost::new(host.clone()),
                    effect.clone(),
                    authorization.clone(),
                ).await.map_err(crate::host_error::from_run_error)
            .expect_err("U6 wrong ownership cannot prepare");
        assert!(
            output.exists(),
            "U6 rejected Preparation keeps physical bytes"
        );
    }
    host.installed_sessions
        .realizations()
        .update_realization(thread, |r| r.environment.owner = retained.clone());
    let mut stale = effect.clone();
    stale.lease.epoch += 1;
    let stale_authorization =
        awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
            stale.clone(),
            projection.workspace_id.clone(),
            None,
        )
        .unwrap();
    awaken_session_execution_contract::TerminalCleanupRuntime::prepare_terminal_cleanup_for_effect(
        &crate::ManagedHost::new(host.clone()),
        stale,
        stale_authorization,
    )
    .await
    .map_err(crate::host_error::from_run_error)
    .expect_err("U6 foreign realization cannot prepare");
    assert!(output.exists(), "U6 stale Preparation keeps physical bytes");
    let preparation = awaken_session_execution_contract::TerminalCleanupRuntime::prepare_terminal_cleanup_for_effect(
                    &crate::ManagedHost::new(host.clone()),
                    effect.clone(),
                    authorization,
                ).await.map_err(crate::host_error::from_run_error)
        .expect("U2 identical Preparation after response loss");
    assert_eq!(first, preparation, "U2 stable receipt");
    let same_owner = || {
        host.installed_sessions
            .realizations()
            .read_realization(thread, |r| match &r.environment.owner {
                SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                    owned: RetiringEnvironmentOwner::Unbound(current),
                    ..
                }) => {
                    Arc::ptr_eq(&current.environment, &candidate.environment)
                        && current.binding == candidate.binding
                        && current.durable_identity().is_none()
                }
                _ => false,
            })
            .unwrap_or(false)
    };
    assert!(same_owner(), "U2 exact original owner");
    aggregate
        .record_terminal_cleanup_preparation(
            &projection.workspace_id,
            &lease,
            preparation,
            Some(
                awaken_session_state_contract::SessionCleanupRepositoryPreparation::new(
                    &awaken_session_types::SessionId::from(thread),
                    &projection.workspace_id,
                    &aggregate.resources,
                )
                .unwrap(),
            ),
        )
        .unwrap();
    host.realizer
        .cleanup
        .acknowledge_terminal_cleanup_preparation(&host.mcp, &effect)
        .await;
    let awaken_session_state_contract::SessionTerminalCleanupAction::Dispose { command } =
        aggregate.terminal_cleanup_work_action().unwrap().unwrap()
    else {
        panic!("U3 aggregate must authorize Disposal")
    };
    let disposal = awaken_session_state_contract::SessionTerminalCleanupDisposalEffect::new(
        command,
        lease.clone(),
    );
    // A Disposal can reconstruct under its own exact operation only when the
    // provider has supplied it. A retained unrelated terminal owner is not that
    // predecessor; neither a valid receipt nor its fingerprint relabels it.
    for wrong in rejected_owners.drain(..) {
        host.installed_sessions
            .realizations()
            .update_realization(thread, |r| r.environment.owner = wrong);
        host.realizer
            .cleanup
            .dispose_terminal_cleanup_effect(&host.realizer, disposal.clone())
            .await
            .expect_err("U6 wrong ownership cannot dispose");
        assert!(output.exists(), "U6 rejected Disposal keeps physical bytes");
    }
    host.installed_sessions
        .realizations()
        .update_realization(thread, |r| r.environment.owner = retained.clone());
    let mut stale_disposal = disposal.clone();
    stale_disposal.lease.epoch += 1;
    host.realizer
        .cleanup
        .dispose_terminal_cleanup_effect(&host.realizer, stale_disposal)
        .await
        .expect_err("U6 foreign realization cannot dispose");
    assert!(same_owner(), "U6 denied effects retain original owner");
    assert!(output.exists(), "U6 stale Disposal keeps physical bytes");
    let first_disposal = host
        .realizer
        .cleanup
        .dispose_terminal_cleanup_effect(&host.realizer, disposal.clone())
        .await
        .expect("U3 exact aggregate-authorized Unbound disposal");
    assert!(!output.exists(), "U3 real physical bytes removed");
    assert!(same_owner(), "U3 retain until durable acknowledgement");
    let receipt = host
        .realizer
        .cleanup
        .dispose_terminal_cleanup_effect(&host.realizer, disposal.clone())
        .await
        .expect("U4 replay physical Disposal after response loss");
    assert_eq!(first_disposal, receipt, "U4 stable disposal receipt");
    assert!(same_owner(), "U4 retained exact owner");
    aggregate
        .record_terminal_cleanup_disposal(
            &projection.workspace_id,
            &lease,
            receipt,
            "unmaterialized replay",
        )
        .unwrap();
    host.realizer
        .cleanup
        .acknowledge_terminal_cleanup_disposal(&host.mcp, &disposal)
        .await;
    assert!(
        !host.installed_sessions.contains(thread),
        "U5 only durable ack releases slot"
    );
}

#[tokio::test]
async fn cold_checkpoint_disposal_cannot_seed_an_owner_without_preparation() {
    // Cause/effect rule: C1 a durable source tuple is exact; C2 the local owner
    // is Vacant because no aggregate-approved Preparation retained it; C3 a
    // typed provider authorization is supplied to the physical-only boundary.
    // Effects: E1 Disposal fails before provider I/O; E2 the owner stays Vacant
    // and no adoption/pending identity is fabricated. R20: C1+C2+C3=>E1+E2.
    // Cold adoption belongs exclusively to the preceding source Preparation
    // phase and is covered end-to-end by the canonical Host continuation tests.
    let thread = "cold-checkpoint-source";
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let generation = generation(thread);
    let binding = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
        "non-resumable",
        thread,
    ))
    .unwrap();
    let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace",
        &awaken_session_types::SessionId::from(thread),
        "suspend",
        &generation,
        9,
        None,
        None,
    );
    let authorization = source_disposal_authorization(&operation.effect_id);
    let error = host
        .realizer
        .dispose_prepared_checkpoint_source_environment(
            thread,
            &operation,
            &generation,
            &binding,
            &authorization,
        )
        .await
        .expect_err("R20/E1 physical Disposal requires retained Preparation");
    assert!(
        error.to_string().contains("no retained Preparation owner"),
        "R20/E1: {error}"
    );
    assert!(
        host.realizer.session_environment_owner_is_vacant(thread),
        "R20/E2"
    );
}
