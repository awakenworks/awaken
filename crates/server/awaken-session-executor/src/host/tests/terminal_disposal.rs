// @formal evidence session_terminal.unstarted_reservation_is_rejected_after_cancel
//! Terminal disposal replay, projection retirement, and terminal quiescence.

use super::*;

/// Terminal cleanup prepares every durability participant before the aggregate
/// opens the one physical-disposal edge. The exact sandbox is reaped only by
/// `SessionRuntime::dispose_terminal_cleanup_for_effect`: preparation leaves
/// its status `Ready`, while disposal flips it to `Terminated`. The process-local
/// terminal projection remains until the aggregate acknowledges the disposal
/// receipt, unlike evict-to-rebuild edges that retain the physical workspace.
#[tokio::test]
async fn exact_terminal_cleanup_disposes_the_threads_sandbox() {
    terminal_cleanup_disposes_exact_owner("resident", false).await;
}

#[tokio::test]
async fn unacknowledged_candidates_complete_terminal_cleanup_without_publication() {
    // U1 root Unmaterialized + unacknowledged Create; U2 root Resident + lost
    // response; U3 root Resident + committed hidden Candidate. Cross U1-U3
    // with retained/revoked ownership. Reuse T1-T4's real physical disposal,
    // concurrent preparation/disposal replay, and aggregate-ack oracles.
    // No row may publish a Resident or fabricate a normal Environment receipt;
    // root-unmaterialized ownership must remain Unbound through terminal ack.
    for initial in ["uncommitted", "lost-response", "committed-hidden"] {
        for revoke in [false, true] {
            terminal_cleanup_disposes_exact_owner(initial, revoke).await;
        }
    }
}

#[tokio::test]
async fn unmaterialized_terminal_cleanup_rejects_known_candidate_binding() {
    // U4: local committed Create or unacknowledged Adoption already knows a
    // durable binding, while the frozen root claims Unmaterialized. Reject both
    // concurrent callers without publication, owner mutation, or physical delete.
    // Missing Adopt response is not missing prior binding provenance.
    for initial in ["committed-conflict", "adoption-conflict"] {
        terminal_cleanup_disposes_exact_owner(initial, false).await;
    }
}

async fn terminal_cleanup_disposes_exact_owner(initial: &str, revoke: bool) {
    use awaken_provisioning_contract::SandboxStatus;
    // Each rule owns its filesystem, including the durable Removed marker.
    // Reusing a thread id across independent rows must not reuse that marker.
    let storage = tempfile::tempdir().expect("terminal owner rule storage");
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await,
    );
    let managed = install_test_dispatch_runtime(&host);
    install_test_session_application(&host);

    let mut terminal_projection = remote_terminal_cleanup_projection();
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        &managed,
        "t-end",
        terminal_projection.clone(),
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("install the complete terminal-test projection before realization");
    // Stage representative resource/config projections before realizing the
    // exact durable V2 Sandbox. Terminal cleanup must erase all of them so
    // reusing the opaque thread id cannot inherit stale scope or model state.
    host.register_thread_memory("t-end", None);
    host.register_thread_resources("t-end", crate::staged_resources::StagedResources::default());
    host.register_thread_model("t-end", "private-model");
    let mut lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "terminal-test-worker".into(),
        runtime_incarnation: "terminal-test-worker:incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    };
    let create_fence = lease
        .sandbox_effect_fence("terminal-test-create")
        .expect("project the exact create fence");
    let physical = host
        .test_local_provider()
        .create_sandbox_for_effect(
            &host.sandbox_spec("t-end"),
            &create_fence,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&create_fence),
        )
        .await
        .expect("create the exact durable terminal-test Sandbox");
    let env = Arc::new(
        crate::session_environment::SandboxEnvironment::workdir(physical)
            .into_session_environment(),
    );
    let binding =
        serde_json::to_string(&env.lifecycle().handle()).expect("encode terminal binding");
    let environment_generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from("t-end"),
        lease.epoch,
        lease.expires_at_unix_ms,
        "terminal-test-environment",
        "terminal-test-image",
    );
    let environment_effect_id = create_fence.operation_id();
    let environment_state = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: binding.clone(),
        effect_id: Some(environment_effect_id.to_owned()),
        generation: Some(environment_generation.clone()),
        idle_since_unix_ms: None,
    };
    let candidate = host
        .begin_session_environment_preparation("t-end", env.clone())
        .expect("retain the exact terminal-test candidate");
    host.install_session_environment_owner_projection(
        "t-end",
        "terminal-workspace",
        &environment_state,
    )
    .expect("project the terminal-test durable Environment identity");
    let identity = crate::session_slot::BoundSessionEnvironmentIdentity::Durable {
        effect_id: environment_effect_id.to_owned(),
        generation: environment_generation,
    };
    match initial {
        "resident" => {
            host.publish_prepared_session_environment("t-end", &candidate, identity)
                .expect("publish the exact terminal-test Environment owner");
            host.ctx_for("t-end", None)
                .await
                .expect("cache the Runtime");
        }
        "committed-hidden" | "committed-conflict" => {
            host.retain_committed_session_environment_candidate("t-end", &candidate, identity)
                .expect("retain actual Store-read identity without publication");
        }
        "adoption-conflict" => {
            // Explicit owner input: an existing binding was adopted, but no
            // new Adopt receipt was read. Do not fabricate durable identity.
            host.session_slots.update("t-end", |slot| {
                let crate::session_slot::SessionEnvironmentOwner::Preparing(
                    crate::session_slot::SessionEnvironmentPreparation::Candidate(current),
                ) = &mut slot.environment_owner
                else {
                    panic!("U4 Candidate input")
                };
                current.origin = crate::session_slot::UnboundSessionEnvironmentOrigin::Adoption;
            });
        }
        "uncommitted" | "lost-response" => {}
        _ => panic!("unknown terminal test row"),
    }
    assert_eq!(
        env.sandbox_environment().status().await.expect("status"),
        SandboxStatus::Ready,
        "the fenced sandbox workspace exists while the Session is live"
    );

    // Cause/effect graph: C1 the application-owned operation freezes one exact
    // root preparation and realization generation; C2 archive and recovery replay
    // that preparation concurrently; C3 the aggregate has not durably admitted
    // preparation; C4 durable preparation projects one typed disposal; C5 physical
    // disposal succeeds but aggregate acknowledgement is absent; C6 the exact
    // acknowledgement arrives; C7 an effect has no installed generation. Effects:
    // E1 preparation is idempotent and leaves the substrate live; E2 no physical
    // mutation crosses C3; E3 exact disposal replay returns one receipt and reaps
    // the substrate; E4 the frozen slot/lease remains retryable; E5 acknowledgement
    // alone retires every local projection; E6 an unfenced/retired effect fails
    // before provider I/O. Constraint: preparation/disposal receipts live only in
    // the aggregate; Host has no combined completion registry or alternate fence.
    // Decision table:
    // | Rule | prep exact/replay | prep durable | disposal exact/replay | ack | stale | Effect |
    // | T1 | T | F | F | F | F | E1 + E2 |
    // | T2 | T | T | T | F | F | E3 + E4 |
    // | T3 | T | T | T | T | F | E5 |
    // | T4 | F | F | F | F | T | E6 |
    let mut cleanup = awaken_session_state_contract::SessionCleanupOperation::default();
    assert!(
        cleanup.request(&awaken_session_types::SessionId::from("t-end")),
        "T1 freezes the terminal fence"
    );
    cleanup
        .freeze_targets(&awaken_session_types::SessionId::from("t-end"), [], 0, 0)
        .expect("T1 freezes the root target");
    let command = cleanup
        .command_for(&awaken_session_types::SessionId::from("t-end"), "t-end")
        .expect("T1 exact root command");
    // A changed aggregate Environment is a new root-CAS snapshot.
    terminal_projection.revision.0 += 1;
    let conflicting = initial.ends_with("-conflict");
    terminal_projection.environment = if initial == "uncommitted" || conflicting {
        awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
    } else {
        environment_state
    };
    if revoke {
        // Install the actual aggregate proof before closing its admission:
        // absence of a response is not an Unowned legacy-disposal permission.
        host.install_test_terminal_cleanup_projection(
            &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
                session_id: "t-end".into(),
                projection: terminal_projection.clone(),
                authority: crate::session_slot::test_lease_receipt(lease.clone()),
            },
        )
        .await
        .expect("U install Managed provenance");
        let lifecycle = host
            .session_slots
            .read("t-end", |slot| slot.lifecycle.clone())
            .unwrap();
        let _lifecycle = lifecycle.lock().await;
        host.retire_session_environment_for_revocation("t-end")
            .await
            .expect("U revoke retained owner");
        // Closed generation is absorbing. Control takeover must issue a new
        // generation; replaying the revoked lease is not a renewal.
        lease.epoch += 1;
    }
    host.install_test_terminal_cleanup_projection(
        &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            session_id: "t-end".into(),
            projection: terminal_projection.clone(),
            authority: crate::session_slot::test_lease_receipt(lease.clone()),
        },
    )
    .await
    .expect("install the exact terminal projection");
    let effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        command.clone(),
        lease.clone(),
    );
    let preparation_authorization =
        terminal_preparation_authorization(&effect, &terminal_projection);
    let before = host.prepared_session_environment("t-end");
    let (archive, recovery) = tokio::join!(
            managed
                .terminal_cleanup()
                .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                    "TerminalCleanupRuntime is unsupported by this runtime"
                ))
                .expect("runtime role")
                .prepare_terminal_cleanup_for_effect(
                    effect.clone(),
                    preparation_authorization.clone(),
                ),
            managed
                .terminal_cleanup()
                .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                    "TerminalCleanupRuntime is unsupported by this runtime"
                ))
                .expect("runtime role")
                .prepare_terminal_cleanup_for_effect(
                    effect.clone(),
                    preparation_authorization.clone(),
                )
        );
    if conflicting {
        for outcome in [archive, recovery] {
            let error = outcome.expect_err("U4 inconsistent root must fail closed");
            assert_eq!(
                error.code,
                "session_terminal_environment_projection_conflict"
            );
        }
        assert!(
            host.prepared_session_environment("t-end")
                .unwrap()
                .exact_matches(&before.unwrap()),
            "U4 retains exact ownership"
        );
        assert_eq!(
            env.sandbox_environment().status().await.unwrap(),
            SandboxStatus::Ready,
            "U4 no physical delete"
        );
        assert!(
            host.session_environment("t-end").await.is_none(),
            "U4 no publication"
        );
        return;
    }
    let archive = archive.unwrap_or_else(|error| {
        panic!("{initial}/{revoke}: archive terminal preparation: {error}")
    });
    let recovery = recovery.expect("recovery terminal preparation replay");
    assert_eq!(archive, recovery, "T1/E1 exact response-loss replay");
    assert_eq!(
        env.sandbox_environment()
            .status()
            .await
            .expect("status after preparation"),
        SandboxStatus::Ready,
        "T1/E2 preparation cannot reap the workspace",
    );
    assert!(
        host.session_slots
            .read("t-end", |slot| {
                matches!(&slot.environment_owner, crate::session_slot::SessionEnvironmentOwner::Retiring(retiring)
                    if Arc::ptr_eq(&retiring.owned.environment(), &env)
                        && retiring.owned.environment().lifecycle().handle() == env.lifecycle().handle())
            })
            .unwrap_or(false),
        "T1/E2 exact owner is hidden from tools but retained for Disposal"
    );
    assert!(
        host.session_environment("t-end").await.is_none(),
        "U no terminal publication"
    );
    if initial == "uncommitted" || initial == "lost-response" {
        assert!(host.session_slots.read("t-end", |slot| matches!(
            &slot.environment_owner,
            crate::session_slot::SessionEnvironmentOwner::Retiring(crate::session_slot::RetiringSessionEnvironment {
                owned: crate::session_slot::RetiringEnvironmentOwner::Unbound(candidate), ..
            }) if candidate.durable_identity().is_none()
        )).unwrap(), "U retains missing ordinary commit evidence");
    }
    managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .acknowledge_terminal_cleanup_preparation(&effect)
        .await;
    assert!(
        host.session_slots.contains("t-end"),
        "T1 root preparation ack is non-retiring"
    );

    let disposal_effect = terminal_disposal_effect(archive, lease, &terminal_projection);
    let (archive, recovery) = tokio::join!(
        managed.dispose_terminal_cleanup_for_effect(disposal_effect.clone()),
        managed.dispose_terminal_cleanup_for_effect(disposal_effect.clone())
    );
    let archive = archive.expect("archive terminal disposal");
    let recovery = recovery.expect("recovery terminal disposal replay");
    assert_eq!(archive, recovery, "T2/E3 exact response-loss replay");

    // The cached ctx is evicted ...
    assert!(
        !host
            .session_slots
            .read("t-end", |slot| slot.runtime.is_some())
            .unwrap_or(false),
        "terminal cleanup evicts the cached ctx"
    );
    assert!(
        host.session_environment("t-end").await.is_none(),
        "terminal end removes the independent environment owner"
    );
    // ... and the sandbox is ACTUALLY disposed: its workspace dir was reaped, so a
    // subsequent status reports Terminated (proving dispose ran, not just an evict).
    assert_eq!(
        env.sandbox_environment().status().await.expect("status"),
        SandboxStatus::Terminated,
        "terminal cleanup disposes the sandbox (workspace reaped), unlike an evict-rebuild"
    );
    assert!(
        host.session_slots.contains("t-end"),
        "T2/E4 retains the exact terminal projection until durable acknowledgement"
    );
    managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .acknowledge_terminal_cleanup_disposal(&disposal_effect)
        .await;
    assert!(host.registered_thread_workspace("t-end").is_none(), "T3/E5");
    assert!(!host.session_slots.contains("t-end"), "T3/E5");
    assert!(
        host.agent.inference_routing.override_for("t-end").is_none(),
        "T3/E5"
    );

    let replay_error = managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .prepare_terminal_cleanup_for_effect(effect, preparation_authorization)
        .await
        .expect_err("T4 retired generation fails closed");
    assert_eq!(
        replay_error.kind,
        awaken_session_error_contract::RunErrorKind::Unavailable
    );
    let mut missing = awaken_session_state_contract::SessionCleanupOperation::default();
    assert!(missing.request(&awaken_session_types::SessionId::from("never-existed")));
    missing
        .freeze_targets(
            &awaken_session_types::SessionId::from("never-existed"),
            [],
            0,
            0,
        )
        .unwrap();
    let missing_effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        missing
            .command_for(
                &awaken_session_types::SessionId::from("never-existed"),
                "never-existed",
            )
            .unwrap(),
        awaken_session_resource_contract::SessionRealizationLease {
            owner: "terminal-test-worker".into(),
            runtime_incarnation: "terminal-test-worker:incarnation".into(),
            epoch: 1,
            expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms()
                + 60_000,
        },
    );
    let missing_authorization =
        awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
            missing_effect.clone(),
            "terminal-workspace".into(),
            None,
        )
        .expect("T4 close the foreign Host preparation input");
    let missing_error = managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .prepare_terminal_cleanup_for_effect(missing_effect, missing_authorization)
        .await
        .expect_err("T4 unknown generation fails closed");
    assert_eq!(
        missing_error.kind,
        awaken_session_error_contract::RunErrorKind::Unavailable
    );
}

#[tokio::test]
async fn restoring_terminal_disposal_replays_until_aggregate_acknowledgement() {
    // Terminal restore-target table TR1. Causes: C1 the frozen aggregate is
    // Restoring and names exact unpublished target R; C2 Preparation has
    // deleted the committed checkpoint and is durably admitted; C3 Disposal
    // physically removes R but its receipt is lost; C4 the same Disposal is
    // replayed; C5 aggregate acknowledgement arrives. Effects: E1 Preparation
    // performs no restore/ordinary Environment observation and retains the
    // exact hidden Restoring request; E2 both Disposal calls use the provider's
    // idempotent exact-target port; E3 response loss retains the same request
    // fence; E4 acknowledgement alone retires the terminal projection. Rules:
    // TR1=C1+C2=>E1; TR2=TR1+C3+C4=>E2+E3; TR3=TR2+C5=>E4. There is no
    // Disposal-local Vacant completion or second receipt registry.
    // TR4: actual committed-response wait x unchanged/Worker revoke/expiry,
    // Session revoke/replacement/renewal/cancellation. Only unchanged/renewal
    // may return a receipt; every failed/cancelled attempt retains the exact
    // Restoring owner. TR5 absent/repeated deletion still crosses both current
    // authorities. These are Host composition checks, not remote rollback.
    for change in [
        "unchanged",
        "worker-revoke",
        "worker-expire",
        "session-revoke",
        "replace",
        "renew",
        "cancel",
    ] {
        let thread = "restoring-terminal-response-loss";
        let (lease, base_effect) = terminal_recovery_effect(thread, "restoring-terminal-worker");
        let generation = awaken_session_state_contract::SandboxGeneration::new(
            &awaken_session_types::SessionId::from(thread),
            lease.epoch,
            lease.expires_at_unix_ms,
            "restoring-terminal-environment",
            "restoring-terminal-image",
        );
        let suspend = awaken_session_state_contract::SessionEnvironmentOperation::new(
            "terminal-workspace",
            &awaken_session_types::SessionId::from(thread),
            "suspend",
            &generation,
            6,
            Some(lease.clone()),
            None,
        );
        let checkpoint = awaken_session_state_contract::SandboxCheckpointRef {
            id: "restoring-terminal-checkpoint".into(),
            format: "awaken-fs-v1".into(),
            digest: "restoring-terminal-checkpoint-digest".into(),
            size_bytes: 1,
            created_at_unix_ms: 1,
            expires_at_unix_ms: u64::MAX,
            environment_fingerprint: generation.environment_fingerprint().to_owned(),
            base_image_fingerprint: generation.base_image_fingerprint().to_owned(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: suspend.effect_id,
        };
        let restore = awaken_session_state_contract::SessionEnvironmentOperation::new(
            "terminal-workspace",
            &awaken_session_types::SessionId::from(thread),
            "restore",
            &generation,
            7,
            Some(lease.clone()),
            Some(&checkpoint),
        );
        let restoring = awaken_session_state_contract::SessionEnvironmentState::Restoring {
            operation: restore,
            checkpoint: checkpoint.clone(),
            generation,
        };
        let restore_target = restoring
            .restoring_request(
                "terminal-workspace",
                &awaken_session_types::SessionId::from(thread),
            )
            .expect("TR1 exact aggregate restore target");
        let effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
            base_effect
                .command
                .with_restore_target(restore_target.clone())
                .expect("TR1 root command owns R"),
            lease.clone(),
        );

        let worker_fence = effect.sandbox_effect_fence().unwrap();
        let worker = Arc::new(pc::test_support::EffectAuthority::new(&worker_fence));
        let probe = Arc::new(TerminalFenceRefreshProbe::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(TerminalRecoveryProvider {
            restored_disposals: Mutex::new(Vec::new()),
            events: events.clone(),
            scenario: TerminalRecoveryScenario::Restoring,
            fence_refresh_probe: Some(probe.clone()),
        });
        let mut host =
            SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub")
                .await
                .with_session_container_provider(
                    provider.clone(),
                    Arc::new(crate::session_environment::UnusedHandExecutorFactory),
                )
                .with_environment_checkpoint_store(Arc::new(RecordingTerminalCheckpointStore {
                    events: events.clone(),
                    expected_id: checkpoint.id,
                    delete_gate: None,
                }));
        host.execution_authority = Some(worker.clone());
        let host = Arc::new(host);
        let managed = install_test_dispatch_runtime(&host);
        install_test_session_application(&host);
        let projection =
            install_terminal_recovery_projection(&host, &managed, thread, lease.clone(), restoring)
                .await;

        let preparation = managed
            .terminal_cleanup()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "TerminalCleanupRuntime is unsupported by this runtime",
                )
            })
            .expect("runtime role")
            .prepare_terminal_cleanup_for_effect(
                effect.clone(),
                terminal_preparation_authorization(&effect, &projection),
            )
            .await
            .expect("TR1 prepare the exact restoring target");
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &["delete-checkpoint"],
            "TR1/E1"
        );
        assert!(matches!(
            host.session_slots
                .read(thread, |slot| slot.environment_owner.clone()),
            Some(crate::session_slot::SessionEnvironmentOwner::Restoring(
                crate::session_slot::SessionEnvironmentRestoration::Awaiting { request }
            )) if request == restore_target
        ));

        let disposal_effect = terminal_disposal_effect(preparation, lease.clone(), &projection);
        let expected_authorization = host
            .terminal_cleanup_disposal_authorization(&disposal_effect)
            .unwrap();

        *worker.current.lock().unwrap() = None;
        assert!(
            managed
                .terminal_cleanup()
                .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                    "TerminalCleanupRuntime is unsupported by this runtime"
                ))
                .expect("runtime role")
                .dispose_terminal_cleanup_for_effect(disposal_effect.clone())
                .await
                .is_err(),
            "TR5 entry denial"
        );
        assert!(provider.restored_disposals.lock().unwrap().is_empty());
        *worker.current.lock().unwrap() =
            Some((worker_fence.clone(), std::time::Duration::from_secs(60)));
        let mut pending =
            Box::pin(managed.dispose_terminal_cleanup_for_effect(disposal_effect.clone()));
        tokio::select! {
            biased;
            result = &mut pending => panic!("TR4 must wait: {result:?}"),
            () = std::future::ready(()) => {}
        }
        assert_eq!(
            provider.restored_disposals.lock().unwrap().len(),
            1,
            "TR4 committed exact call before response"
        );
        match change {
            "worker-revoke" => *worker.current.lock().unwrap() = None,
            "worker-expire" => {
                worker.current.lock().unwrap().as_mut().unwrap().1 = std::time::Duration::ZERO
            }
            "session-revoke" => host.session_slots.update(thread, |slot| {
                slot.realization_authority =
                    crate::session_slot::SessionRealizationAuthority::Closed(Some(lease.clone()));
            }),
            "replace" | "renew" => host.session_slots.update(thread, |slot| {
                let mut next = lease.clone();
                if change == "replace" {
                    next.epoch += 1;
                }
                next.expires_at_unix_ms += 10_000;
                slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
                    crate::session_slot::test_observed_lease(next),
                );
            }),
            _ => {}
        }
        if change == "cancel" {
            drop(pending);
        } else {
            probe.restored_release.notify_one();
            assert_eq!(
                pending.await.is_ok(),
                matches!(change, "unchanged" | "renew"),
                "TR4 {change}"
            );
        }
        assert!(
            matches!(host.session_slots.read(thread, |slot| slot.environment_owner.clone()),
        Some(crate::session_slot::SessionEnvironmentOwner::Restoring(
            crate::session_slot::SessionEnvironmentRestoration::Awaiting { request }
        )) if request == restore_target),
            "TR4 retain original target, not Vacant"
        );
        assert_eq!(
            provider.restored_disposals.lock().unwrap().as_slice(),
            std::slice::from_ref(&expected_authorization)
        );
        // Begin the independent TR2/TR3 exact-retry phase only after checking the
        // late response and original owner. Reset fixture observations and supplied
        // proof; production renewal cannot revive a revoked incarnation.
        provider.restored_disposals.lock().unwrap().clear();
        events.lock().unwrap().truncate(1);
        *worker.current.lock().unwrap() =
            Some((worker_fence.clone(), std::time::Duration::from_secs(60)));
        host.session_slots.update(thread, |slot| {
            slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
                crate::session_slot::test_observed_lease(lease.clone()),
            );
        });
        probe.restored_release.notify_one();
        let first = managed
            .terminal_cleanup()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "TerminalCleanupRuntime is unsupported by this runtime",
                )
            })
            .expect("runtime role")
            .dispose_terminal_cleanup_for_effect(disposal_effect.clone())
            .await
            .expect("TR2 dispose exact restore target");
        probe.restored_release.notify_one();
        let replay = managed
            .terminal_cleanup()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "TerminalCleanupRuntime is unsupported by this runtime",
                )
            })
            .expect("runtime role")
            .dispose_terminal_cleanup_for_effect(disposal_effect.clone())
            .await
            .expect("TR2 replay after disposal receipt loss");
        assert_eq!(first, replay, "TR2/E2 deterministic receipt");
        // TR2A: the exact root authorization reaches the restore-target port on
        // both initial deletion and receipt-loss replay, with no provider synthesis.
        assert_eq!(
            *provider.restored_disposals.lock().unwrap(),
            vec![expected_authorization.clone(), expected_authorization]
        );

        assert_eq!(
            events.lock().unwrap().as_slice(),
            &["delete-checkpoint", "dispose-restored", "dispose-restored"],
            "TR2/E2 exact provider port handles absence replay"
        );
        assert!(
            matches!(
                host.session_slots
                    .read(thread, |slot| slot.environment_owner.clone()),
                Some(crate::session_slot::SessionEnvironmentOwner::Restoring(
                    crate::session_slot::SessionEnvironmentRestoration::Awaiting { request }
                )) if request == restore_target
            ),
            "TR2/E3 response-loss fence"
        );

        managed
            .terminal_cleanup()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "TerminalCleanupRuntime is unsupported by this runtime",
                )
            })
            .expect("runtime role")
            .acknowledge_terminal_cleanup_disposal(&disposal_effect)
            .await;
        assert!(!host.session_slots.contains(thread), "TR3/E4");
    }
}

#[tokio::test]
async fn terminal_projection_retirement_requires_generation_tag_and_durable_absence() {
    // Cause/effect graph: C1 root lease generation is exact; C2 child c1 has
    // one durably accepted exact preparation; C3 child c2 remains pending; C4
    // the same opaque child id has a foreign
    // replacement tag; C5 a child/root preparation is acknowledged; C6 Control
    // reports aggregate completion. Effects: E1 retire c1 only; E2 retain root+c2;
    // E3 never remove the foreign replacement; E4 root preparation acknowledgement
    // is non-retiring; E5 completed readback retires the exact tree children-first.
    // Constraint: this edge forgets process-local projection only and performs
    // no provider, Resource, or aggregate mutation.
    // Decision table:
    // | Rule | exact gen | root tag | prep target | complete | Effect |
    // | P1 | T | T | exact child | F | E1 |
    // | P2 | T | T | another child | F | E2 |
    // | P3 | T | F | child | F | E3 |
    // | P4 | T | T | root | F | E4 |
    // | P5 | T | T | none | T | E5 |
    let host = SharedHost::new(Arc::new(OkModel), "stub").await;
    let session_id = "terminal-retire-root";
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "terminal-retire-worker".into(),
        runtime_incarnation: "terminal-retire-worker:incarnation".into(),
        epoch: 7,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    };
    host.install_test_terminal_cleanup_projection(
        &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            session_id: session_id.into(),
            projection: remote_terminal_cleanup_projection(),
            authority: crate::session_slot::test_lease_receipt(lease.clone()),
        },
    )
    .await
    .expect("P1-P4 install the aggregate-derived root projection");
    for child in ["terminal-retire-done", "terminal-retire-pending"] {
        host.register_thread_workspace(child, "terminal-retire-workspace");
        host.session_slots.update(child, |slot| {
            slot.terminal_cleanup_root = Some(session_id.into());
        });
    }
    let foreign = "terminal-retire-foreign";
    host.register_thread_workspace(foreign, "foreign-workspace");
    host.session_slots.update(foreign, |slot| {
        slot.terminal_cleanup_root = Some("replacement-root".into());
    });

    let mut cleanup = awaken_session_state_contract::SessionCleanupOperation::default();
    assert!(cleanup.request(&awaken_session_types::SessionId::from(session_id)));
    cleanup
        .freeze_targets(
            &awaken_session_types::SessionId::from(session_id),
            [
                "terminal-retire-done".to_string(),
                "terminal-retire-pending".to_string(),
                foreign.to_string(),
            ],
            0,
            0,
        )
        .unwrap();
    let completed_child = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        cleanup
            .command_for(
                &awaken_session_types::SessionId::from(session_id),
                "terminal-retire-done",
            )
            .unwrap(),
        lease.clone(),
    );
    host.acknowledge_terminal_cleanup_preparation(&completed_child)
        .await;
    assert!(
        !host.session_slots.contains("terminal-retire-done"),
        "P1/E1"
    );
    assert!(host.session_slots.contains(session_id), "P2/E2");
    assert!(
        host.session_slots.contains("terminal-retire-pending"),
        "P2/E2"
    );
    assert!(host.session_slots.contains(foreign), "P3/E3");

    let stale_foreign = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        cleanup
            .command_for(&awaken_session_types::SessionId::from(session_id), foreign)
            .unwrap(),
        lease.clone(),
    );
    host.acknowledge_terminal_cleanup_preparation(&stale_foreign)
        .await;
    assert!(host.session_slots.contains(foreign), "P3/E3");

    let root = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        cleanup
            .command_for(
                &awaken_session_types::SessionId::from(session_id),
                session_id,
            )
            .unwrap(),
        lease,
    );
    host.acknowledge_terminal_cleanup_preparation(&root).await;
    assert!(host.session_slots.contains(session_id), "P4/E4");
    host.acknowledge_completed_terminal_cleanup(session_id, &root.lease)
        .await;
    assert!(!host.session_slots.contains(session_id), "P5/E5");
    assert!(
        !host.session_slots.contains("terminal-retire-pending"),
        "P5/E5"
    );
    assert!(host.session_slots.contains(foreign), "P4/E3");
}

#[tokio::test]
async fn terminal_quiescence_never_materializes_a_cold_environment() {
    let host = SharedHost::new(Arc::new(OkModel), "stub").await;

    let snapshot = host
        .quiesce_terminal_delegations("cold-terminal")
        .await
        .expect("cold terminal fence reads committed truth");

    assert!(snapshot.delegated_runs.is_empty());
    assert_eq!(snapshot.watermark, 0);
    assert!(
        host.session_slots
            .read("cold-terminal", |slot| {
                slot.runtime.is_none() && !slot.environment_owner.has_local_environment()
            })
            .unwrap_or(true),
        "terminal control may allocate a lock slot but not a Runtime or Environment projection"
    );
}

#[tokio::test]
async fn terminal_quiescence_rejects_an_unstarted_cancelled_reservation_without_waiting_for_ttl() {
    // Cause/effect decision table for the admission/termination race: C1 a
    // self-affine Session Run remains Reserved inside its exclusive 30-second
    // admission window; C2 terminal Session truth starts quiescence before the
    // activity receipt activates that row. Effects: E1 cancellation is durably
    // ordered first; E2 the existing guarded reservation rejection removes the
    // unstarted row without a fabricated Run completion; E3 quiescence returns
    // without waiting for the ordinary reservation-recovery TTL and does not
    // materialize an Environment. Rule T1=C1+C2=>E1+E2+E3. If activation wins,
    // guarded rejection stutters and the existing Pending cancellation Worker
    // owns settlement; the sibling parent/child tests cover that branch.
    use awaken_dispatch_contract::DispatchQueue as _;

    let host = SharedHost::new(Arc::new(OkModel), "stub").await;
    let dispatch = host.dispatch_store().expect("T1 dispatch authority");
    let session = ThreadId("terminal-reservation-session".into());
    let run = RunId("terminal-reservation-run".into());
    let activation =
        crate::host::worker_resolver::test_support::test_activation(&session.0, &run.0);
    let command = awaken_session_execution_contract::AdmitSessionRun {
        session_id: SessionId::from(&session.0),
        agent_id: activation.snapshot.root_agent_id.0.clone(),
        operation_id: "terminal-reservation-operation".into(),
        run_id: run.clone(),
        messages: activation.input.clone(),
        data_subject_id: activation
            .data_subject_id
            .as_ref()
            .map(|subject| subject.0.clone()),
        traceparent: None,
        execution_requirements: awaken_session_execution_contract::SessionRunExecutionRequirements {
            tool_capability_narrowing: activation.tool_capability_narrowing(),
            required_worker_capabilities: Default::default(),
        },
        replacement: awaken_session_execution_contract::SessionRunReplacement::PreservePrior,
    };
    let request = awaken_dispatch_contract::RunDispatch::new(activation)
        .for_session(session.clone())
        .with_session_command_fingerprint(
            awaken_session_execution_contract::SessionRunCommandFingerprint::current(&command),
        );
    assert_eq!(
        dispatch
            .reserve_session_run(request, 30_000)
            .await
            .expect("T1/C1 reserve"),
        awaken_dispatch_contract::SessionRunReservationOutcome::Reserved,
        "T1/C1"
    );

    tokio::time::timeout(
        std::time::Duration::from_millis(250),
        host.quiesce_terminal_delegations(&session.0),
    )
    .await
    .expect("T1/E3 does not wait for reservation TTL")
    .expect("T1 terminal quiescence");

    assert!(
        dispatch
            .list_dispatches()
            .await
            .expect("T1 inspect dispatch")
            .is_empty(),
        "T1/E1+E2"
    );
    assert!(
        host.session_environment(&session.0).await.is_none(),
        "T1/E3"
    );
}

async fn terminal_physical_claim_fixture(
    label: &str,
    ttl_ms: u64,
) -> (
    Arc<SharedHost>,
    Arc<awaken_dispatch_erasure::AnyDispatchStore>,
    Arc<awaken_lease_testkit::ManualClock>,
    ThreadId,
    RunId,
    awaken_dispatch_contract::RunClaim,
) {
    use awaken_dispatch::{DispatchQueue, RunDispatch};
    use awaken_dispatch_contract::RunClaim;

    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("physical Dispatch fixture"),
    );
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .await
            .with_coordinator_dispatch_store(dispatch.clone()),
    );
    let session = ThreadId(format!("terminal-{label}-physical-session"));
    let run = RunId(format!("terminal-{label}-physical-run"));
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &session.0, &run.0,
            ))
            .for_session(session.clone())
            .with_session_activity_epoch(1),
        )
        .await
        .unwrap();
    let claimed = dispatch
        .claim_run(&run, "old-worker", ttl_ms, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let physical_claim = RunClaim::from(&claimed.lease);
    dispatch.begin_attempt(&physical_claim, None).await.unwrap();
    (host, dispatch, clock, session, run, physical_claim)
}

#[test]
fn terminal_quiescence_and_thread_archive_share_the_exact_unsettled_states() {
    // C1 one of the seven durable Dispatch states; E1 the five still-runnable
    // states prohibit quiescence, while DeadLetter and Superseded do not.
    // Rules S1=Reserved/ReservationLeased/Pending/Leased/Awaiting=>true;
    // S2=DeadLetter/Superseded=>false. Both callers use the same predicate.
    use awaken_dispatch_contract::DispatchState as S;
    for (state, expected) in [
        (S::Reserved, true),
        (S::ReservationLeased, true),
        (S::Pending, true),
        (S::Leased, true),
        (S::Awaiting, true),
        (S::DeadLetter, false),
        (S::Superseded, false),
    ] {
        assert_eq!(
            crate::host::dispatch_remains_unsettled(state),
            expected,
            "S1/S2"
        );
    }
}

#[tokio::test]
async fn terminal_quiescence_does_not_rewait_an_expired_physical_claim() {
    // C1 the terminal Session owns one still-occupied physical Dispatch slot;
    // C2 its exact mutation Claim has expired; C3 no independent stop receipt
    // has retired that slot. E1 durable cancellation is retained; E2 neither
    // of two cold recovery passes waits the full normal settlement window;
    // E3 the physical slot is never cleared or treated as a successful cleanup.
    // Rules Q1=C1+C2+C3=>E1+E2+E3; Q2 in the sibling test retains the
    // ordinary wait for a Claim that was live before cancellation. Expiry is
    // supplied by the Dispatch clock, not a sleep or a caller assertion.
    use awaken_dispatch::{ControlDispatchQueue, DispatchQueue};

    let (host, dispatch, clock, session, run, physical_claim) =
        terminal_physical_claim_fixture("expired", 100).await;
    clock.advance(101);
    assert!(
        !dispatch.claim_is_current(&physical_claim).await.unwrap(),
        "Q1/C2"
    );

    for _ in 0..2 {
        let failure = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            host.quiesce_terminal_delegations(&session.0),
        )
        .await
        .expect("Q1/E2 expired claim must not consume the normal ten-second wait")
        .expect_err("Q1/E3 no stop receipt means cleanup stays pending");
        assert!(
            failure.to_string().contains("physical attempt"),
            "Q1/E3: {failure}"
        );
        let row = dispatch.list_dispatches().await.unwrap().pop().unwrap();
        assert!(
            row.cancellation_requested && row.physical_attempt_active,
            "Q1/E1+E3"
        );
        assert_eq!(
            dispatch
                .load_physical_attempt(&run)
                .await
                .unwrap()
                .unwrap()
                .claim,
            physical_claim,
            "Q1/E3 only the original Dispatch authority may retire the slot"
        );
    }
}

#[tokio::test]
async fn terminal_quiescence_still_waits_for_a_pre_cancel_live_physical_claim() {
    // C1 the physical Claim is current before Session cancellation; C2 that
    // cancellation revokes the mutation lease; C3 the same old Worker later
    // retires only its physical slot; C4 a fresh cancellation Claim settles.
    // E1 terminal cleanup does not mistake C2 for prior staleness; E2 it waits
    // until C3+C4, then succeeds; E3 no physical slot or runnable row remains.
    // Rule Q2=C1+C2+C3+C4=>E1+E2+E3. Q1 covers pre-cancel expiry instead.
    use awaken_dispatch::{DispatchOutcome, DispatchQueue};

    let (host, dispatch, _clock, session, run, physical_claim) =
        terminal_physical_claim_fixture("live", 30_000).await;
    assert!(
        dispatch.claim_is_current(&physical_claim).await.unwrap(),
        "Q2/C1"
    );
    let terminal = tokio::spawn({
        let host = host.clone();
        let session = session.clone();
        async move { host.quiesce_terminal_delegations(&session.0).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if dispatch
                .list_dispatches()
                .await
                .unwrap()
                .iter()
                .any(|row| row.run_id == run && row.cancellation_requested)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Q2/C2 cancellation becomes durable");
    assert!(
        !terminal.is_finished(),
        "Q2/E1 normal physical work still blocks"
    );
    dispatch.finish_attempt(&physical_claim).await.unwrap();
    let cancellation = dispatch
        .claim_run(&run, "cancellation-worker", 30_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    dispatch
        .settle(&run, cancellation.lease.epoch, DispatchOutcome::Done, &[])
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), terminal)
        .await
        .expect("Q2/E2 exact settlement completes before deadline")
        .unwrap()
        .expect("Q2/E2 terminal quiescence succeeds");
    assert!(
        dispatch.list_dispatches().await.unwrap().is_empty(),
        "Q2/E3"
    );
}

async fn settle_cancelled_dispatch_rows(
    dispatch: Arc<awaken_dispatch_erasure::AnyDispatchStore>,
    expected_rows: usize,
    worker: &'static str,
) -> Vec<RunId> {
    use awaken_dispatch::{DispatchOutcome, DispatchQueue as _};

    loop {
        let rows = dispatch.list_dispatches().await.unwrap();
        if rows.len() == expected_rows && rows.iter().all(|row| row.cancellation_requested) {
            let mut settled = Vec::with_capacity(rows.len());
            for row in rows {
                let claimed = dispatch
                    .claim_run(&row.run_id, worker, 30_000, &Default::default())
                    .await
                    .unwrap()
                    .expect("cancelled dispatch remains claimable for exact settlement");
                dispatch
                    .settle(&row.run_id, claimed.lease.epoch, DispatchOutcome::Done, &[])
                    .await
                    .unwrap();
                settled.push(row.run_id);
            }
            return settled;
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn terminal_quiescence_fences_ephemeral_children_before_cold_link_enrichment() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the Session has no resident Runtime or recoverable
    // current publication; C2 an ephemeral runtime authority owns a parent-affined
    // child row; C3 deployment.durable is false; C4 terminal quiescence starts.
    // C5 the child Worker observes the cancellation bit and settles its exact
    // epoch. E1 the authority-backed queue cancellation fence precedes settlement;
    // E2 the logical child Thread is frozen into cleanup after its row disappears;
    // E3 no Environment is built; E4 quiescence returns only after settlement.
    // Decision table: R1(C1+C2+C3+C4+C5)->E1+E2+E3+E4. The durable store case is
    // exercised by the terminal cleanup integration tests, while the sibling
    // cold-empty test owns !C2. This rule prevents persistence mode from
    // masquerading as dispatch authority ownership.
    let host = SharedHost::new(Arc::new(OkModel), "stub").await;
    assert!(!host.deployment.durable, "R1/C3");
    let dispatch = host
        .dispatch_store()
        .expect("R1 ephemeral dispatch authority");
    let parent = ThreadId("cold-terminal-parent".into());
    let child = ThreadId("cold-terminal-child".into());
    let child_run = RunId("cold-terminal-child-run".into());
    dispatch
        .enqueue(
            awaken_dispatch_contract::RunDispatch::new(
                crate::host::worker_resolver::test_support::test_activation(&child.0, &child_run.0),
            )
            .for_session(parent.clone()),
        )
        .await
        .expect("R1 durable child admission");
    let worker = tokio::spawn(settle_cancelled_dispatch_rows(
        dispatch.clone(),
        1,
        "ephemeral-terminal-worker",
    ));
    let snapshot = host
        .quiesce_terminal_delegations(&parent.0)
        .await
        .expect("R1 terminal fence waits for exact child settlement");
    let settled = worker.await.unwrap();

    assert_eq!(snapshot.coordinated_thread_ids, vec![child], "R1/E2");
    assert_eq!(settled, vec![child_run], "R1/E1+E4");
    let rows = dispatch.list_dispatches().await.expect("R1 inspect fence");
    assert!(rows.is_empty(), "R1/E4");
    assert!(
        host.session_slots
            .read(&parent.0, |slot| slot.runtime.is_none()
                && !slot.environment_owner.has_local_environment())
            .unwrap_or(true),
        "R1/E3"
    );
}

#[tokio::test]
async fn coordinator_terminal_quiescence_waits_for_remote_parent_and_child_settlement() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 Coordinator owns the durable queue but no local
    // pool; C2 root and parent-affined child are active; C3 cancellation intent
    // is durable but the remote Worker has not settled it; C4 the remote Worker
    // settles both exact epochs. Effects: E1 quiescence does not freeze early;
    // E2 both rows are cancelled; E3 the child identity remains in the frozen
    // snapshot after its queue row disappears; E4 no Environment is built.
    //
    // | Rule | topology | cancellation | settlement | Effect |
    // | Q1 | coordinator-only | requested | pending | E1 + E2 |
    // | Q2 | coordinator-only | requested | exact Done | E3 + E4 |
    use awaken_dispatch::{DispatchQueue, RunDispatch};

    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(system_clock())
            .await
            .expect("remote quiescence dispatch"),
    );
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .await
            .with_coordinator_dispatch_store(dispatch.clone()),
    );
    let parent = ThreadId("remote-quiescence-parent".into());
    let child = ThreadId("remote-quiescence-child".into());
    for (thread, run) in [
        (&parent, RunId("remote-quiescence-root-run".into())),
        (&child, RunId("remote-quiescence-child-run".into())),
    ] {
        dispatch
            .enqueue(
                RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                    &thread.0, &run.0,
                ))
                .for_session(parent.clone())
                .with_session_activity_epoch(1),
            )
            .await
            .expect("Q1 active dispatch");
    }
    let remote = tokio::spawn(settle_cancelled_dispatch_rows(
        dispatch.clone(),
        2,
        "remote-worker",
    ));

    let snapshot = host
        .quiesce_terminal_delegations(&parent.0)
        .await
        .expect("Q2 remote settlement proves quiescence");
    assert_eq!(remote.await.unwrap().len(), 2, "Q1/E1");
    assert_eq!(snapshot.coordinated_thread_ids, vec![child], "Q2/E3");
    assert!(
        dispatch.list_dispatches().await.unwrap().is_empty(),
        "Q2/E2"
    );
    assert!(host.session_environment(&parent.0).await.is_none(), "Q2/E4");
}
