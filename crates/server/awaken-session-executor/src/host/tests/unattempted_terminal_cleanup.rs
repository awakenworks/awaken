use super::*;

#[tokio::test]
async fn cold_unmaterialized_recovery_uses_the_saved_create_not_the_cleanup_lease() {
    // C1 original first Create is saved by the controlled aggregate; C2 cleanup
    // is delivered by another owner/incarnation/epoch; C3 historical omission.
    // R1=C1+C2 => Provider receives exact original operation/fence, not C2's
    // reconstructed Create. R2=C3+C2 => typed pending before any Provider input.
    // The provider observation is a selected test port; these rows certify the
    // cold input boundary, not foreign physical takeover or actual absence.
    for known in [false, true] {
        let thread = "cold-original-create";
        let mut projection = remote_terminal_cleanup_projection();
        let old_lease = crate::session_resource::SessionRealizationLease {
            owner: "old-worker".into(),
            runtime_incarnation: "old-boot".into(),
            epoch: 3,
            expires_at_unix_ms: 500,
        };
        let original = awaken_session_contract::SessionEnvironmentEffectIntent::new(
            thread,
            awaken_session_contract::SessionEnvironmentEffectKind::Create,
            Some(old_lease.clone()),
        )
        .for_environment(projection.baseline.environment.config_fingerprint.0.clone());
        if known {
            let saved = fresh_terminal_fixture_before_create(thread, &mut projection, &old_lease);
            assert_eq!(
                saved.environment_materialization().unwrap().pending(),
                Some(&original),
                "R1 actual original admission"
            );
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::new(TerminalFenceRefreshProbe::default());
        let provider = Arc::new(TerminalRecoveryProvider {
            restored_disposals: Mutex::new(Vec::new()),
            events: events.clone(),
            scenario: TerminalRecoveryScenario::TotalAbsence,
            fence_refresh_probe: Some(probe.clone()),
        });
        let (lease, effect) = terminal_recovery_effect(thread, "new-cleanup-worker");
        let fence = effect.sandbox_effect_fence().unwrap();
        let (host, _managed) = cold_host(provider, &fence).await;
        let assignment = awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection,
            environment_creation_not_attempted: false,
            authority: awaken_session_realization::test_support::test_lease_receipt(lease),
        };
        host.run_assembly
            .installed_sessions
            .install_test_terminal_cleanup_projection(
                (
                    &host.run_assembly.realization_executor,
                    &host.run_assembly.mcp,
                ),
                &assignment,
            )
            .await
            .unwrap();
        let result = awaken_session_execution_contract::TerminalCleanup::prepare_terminal_cleanup_for_effect(
                    &*(host.clone()).run_assembly,
                    effect.clone(),
                    terminal_preparation_authorization(&effect, &assignment.projection),
                ).await.map_err(crate::session_execution_error::from_run_error);
        if known {
            result.unwrap();
            let saved = old_lease
                .sandbox_effect_fence(original.effect_id())
                .unwrap();
            assert_eq!(
                probe.fences.lock().unwrap().as_slice(),
                &[("original-create-input", saved)],
                "R1 exact original identity"
            );
            assert_ne!(old_lease.owner, fence.owner(), "R1 changed cleanup owner");
        } else {
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("R2 missing history became an input"),
            };
            assert_eq!(
                error.code, "session_terminal_unmaterialized_origin_missing",
                "R2 pending"
            );
            assert!(
                probe.fences.lock().unwrap().is_empty(),
                "R2 no Provider input"
            );
            assert!(events.lock().unwrap().is_empty(), "R2 no Provider effect");
        }
    }
}

async fn cold_host(
    provider: Arc<TerminalRecoveryProvider>,
    fence: &pc::SandboxEffectFence,
) -> (
    Arc<crate::ExecutorComponents>,
    Arc<crate::ExecutorComponents>,
) {
    let mut deps = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .with_session_container_provider(
        provider,
        Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
    );
    deps.execution_authority = Some(Arc::new(pc::test_support::EffectAuthority::new(fence)));
    let host = deps.build().unwrap();
    let managed = host.clone();
    install_test_session_application(&host);
    (host, managed)
}

#[tokio::test]
async fn cold_terminal_cleanup_consumes_only_control_no_create_evidence() {
    // Cross-process decision table: H1 Control's exact archive witness +
    // Unmaterialized/Vacant -> zero provider Create/adopt/observe/prepare/delete,
    // ordinary participants and canonical preparation/disposal remain required.
    // H2 absent historical witness -> the existing unknown provider rejects;
    // H3 witness + resident projection -> conflict without effect. H4 a new
    // Host with only the reissued assignment can finish canonical Disposal.
    // H5 historical wire omission -> false; explicit true survives transport.
    // H6 the rebased assignment uses the installed-Session receipt fixture;
    // the existing H1-H5 effects must compile and run without a legacy alias.
    for witness in [false, true] {
        let thread = if witness {
            "no-create-known"
        } else {
            "no-create-unknown"
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(TerminalRecoveryProvider {
            restored_disposals: Mutex::new(Vec::new()),
            events: events.clone(),
            scenario: TerminalRecoveryScenario::OrphanClaim,
            fence_refresh_probe: None,
        });
        let (lease, effect) = terminal_recovery_effect(thread, "no-create-worker");
        let worker_fence = effect.sandbox_effect_fence().unwrap();
        let (host, managed) = cold_host(provider.clone(), &worker_fence).await;
        let projection = remote_terminal_cleanup_projection();
        let assignment = awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection: projection.clone(),
            environment_creation_not_attempted: witness,
            authority: awaken_session_realization::test_support::test_lease_receipt(lease.clone()),
        };
        let wire = serde_json::to_value(&assignment).unwrap();
        assert_eq!(
            wire.get("environment_creation_not_attempted").is_some(),
            witness,
            "H5 historical omission"
        );
        let assignment: awaken_session_realization_contract::SessionTerminalCleanupAssignment =
            serde_json::from_value(wire).unwrap();
        assert_eq!(
            assignment.environment_creation_not_attempted, witness,
            "H5 exact wire meaning"
        );
        host.run_assembly
            .installed_sessions
            .install_test_terminal_cleanup_projection(
                (
                    &host.run_assembly.realization_executor,
                    &host.run_assembly.mcp,
                ),
                &assignment,
            )
            .await
            .unwrap();
        let result = awaken_session_execution_contract::TerminalCleanup::prepare_terminal_cleanup_for_effect(
                    &*(host.clone()).run_assembly,
                    effect.clone(),
                    terminal_preparation_authorization(&effect, &projection),
                ).await.map_err(crate::session_execution_error::from_run_error);
        if !witness {
            assert!(
                result.is_err(),
                "H2 missing evidence does not become absence"
            );
            continue;
        }
        let preparation = result.expect("H1 known no-Create preparation");
        let disposal = terminal_disposal_effect(preparation, lease.clone(), &projection);
        drop(managed);
        drop(host);
        let (host, _managed) = cold_host(provider, &worker_fence).await;
        host.run_assembly
            .installed_sessions
            .install_test_terminal_cleanup_projection(
                (
                    &host.run_assembly.realization_executor,
                    &host.run_assembly.mcp,
                ),
                &assignment,
            )
            .await
            .unwrap();
        host.realization_executor
            .effects()
            .dispose_terminal_cleanup_effect(host.realization_executor.peers(), disposal.clone())
            .await
            .expect("H1/H4 exact disposal without guessed provider I/O");
        assert!(events.lock().unwrap().is_empty(), "H1 no provider effects");
        let mut conflicting = assignment;
        conflicting.projection.revision.0 += 1;
        conflicting.projection.environment =
            awaken_session_contract::SessionEnvironmentState::Resident {
                binding: "foreign".into(),
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            };
        // A contradictory projection cannot consume the no-Create branch.
        host.run_assembly
            .installed_sessions
            .install_test_terminal_cleanup_projection(
                (
                    &host.run_assembly.realization_executor,
                    &host.run_assembly.mcp,
                ),
                &conflicting,
            )
            .await
            .unwrap();
        let error = awaken_session_execution_contract::TerminalCleanup::prepare_terminal_cleanup_for_effect(
                    &*(host.clone()).run_assembly,
                    effect.clone(),
                    terminal_preparation_authorization(&effect, &conflicting.projection),
                ).await.map_err(crate::session_execution_error::from_run_error)
            .expect_err("H3 contradictory projection");
        assert_eq!(
            error.code, "session_terminal_unattempted_environment_conflict",
            "H3 conflict"
        );
        assert!(events.lock().unwrap().is_empty(), "H3 no provider effects");
    }
}
