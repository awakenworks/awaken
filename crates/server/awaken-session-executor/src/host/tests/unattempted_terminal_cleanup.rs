use super::*;

async fn cold_host(
    provider: Arc<TerminalRecoveryProvider>,
    fence: &pc::SandboxEffectFence,
) -> (Arc<crate::SharedHost>, crate::ManagedHost) {
    let mut deps = crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .with_session_container_provider(
        provider,
        Arc::new(crate::session_environment::UnusedHandExecutorFactory),
    );
    deps.execution_authority = Some(Arc::new(pc::test_support::EffectAuthority::new(fence)));
    let host = deps.build().unwrap();
    let managed = install_test_dispatch_runtime(&host);
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
            authority: crate::installed_session::test_lease_receipt(lease.clone()),
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
        host.install_test_terminal_cleanup_projection(&assignment)
            .await
            .unwrap();
        let result = host
            .prepare_terminal_cleanup_effect(
                effect.clone(),
                terminal_preparation_authorization(&effect, &projection),
            )
            .await;
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
        host.install_test_terminal_cleanup_projection(&assignment)
            .await
            .unwrap();
        host.dispose_terminal_cleanup_effect(disposal.clone())
            .await
            .expect("H1/H4 exact disposal without guessed provider I/O");
        assert!(events.lock().unwrap().is_empty(), "H1 no provider effects");
        let mut conflicting = assignment;
        conflicting.projection.revision.0 += 1;
        conflicting.projection.environment =
            awaken_session_state_contract::SessionEnvironmentState::Resident {
                binding: "foreign".into(),
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            };
        // A contradictory projection cannot consume the no-Create branch.
        host.install_test_terminal_cleanup_projection(&conflicting)
            .await
            .unwrap();
        let error = host
            .prepare_terminal_cleanup_effect(
                effect.clone(),
                terminal_preparation_authorization(&effect, &conflicting.projection),
            )
            .await
            .expect_err("H3 contradictory projection");
        assert_eq!(
            error.code, "session_terminal_unattempted_environment_conflict",
            "H3 conflict"
        );
        assert!(events.lock().unwrap().is_empty(), "H3 no provider effects");
    }
}
