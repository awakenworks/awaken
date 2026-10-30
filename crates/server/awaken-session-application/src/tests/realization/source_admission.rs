//! Terminal source-read authorization and its Worker protocol admission.

use super::*;

/// Original source-free test receipts, shared by retirement and cold admission.
pub(super) fn prepare_terminal_root_for_disposal(root: &mut PersistedSession) {
    let lease = root.realization.clone().unwrap();
    let commands = match root.terminal_cleanup_work_action().unwrap().unwrap() {
        awaken_session_contract::SessionTerminalCleanupAction::Prepare { commands } => commands,
        other => panic!("original preparation: {other:?}"),
    };
    for command in commands {
        let effect =
            awaken_session_contract::SessionTerminalCleanupEffect::new(command, lease.clone());
        let prepared = awaken_session_contract::SessionCleanupPreparation::try_new(
            &effect,
            effect.sandbox_effect_fence().unwrap(),
            Vec::new(),
        )
        .unwrap();
        let repository = awaken_session_contract::SessionCleanupRepositoryPreparation::new(
            &root.session_id,
            "workspace",
            &root.resources,
        )
        .unwrap();
        root.record_terminal_cleanup_preparation("workspace", &lease, prepared, Some(repository))
            .unwrap();
    }
    assert!(matches!(
        root.terminal_cleanup_work_action().unwrap(),
        Some(awaken_session_contract::SessionTerminalCleanupAction::Dispose { .. })
    ));
}

pub(super) fn terminal_disposal_action(
    session_id: &str,
) -> awaken_session_contract::SessionTerminalCleanupAction {
    use awaken_provisioning_contract::{SandboxDisposalPreparation, SandboxEffectFence};
    awaken_session_contract::SessionTerminalCleanupAction::Dispose {
        command: awaken_session_contract::SessionCleanupDisposalCommand {
            session_id: session_id.into(),
            effect_id: "terminal".into(),
            preparation_fingerprint: "fixture-preparation".into(),
            provider_disposal: SandboxDisposalPreparation::new(
                SandboxEffectFence::new("terminal", "worker", "boot", 1, u64::MAX).unwrap(),
                "fixture-provider",
            )
            .unwrap(),
            restore_target: None,
        },
    }
}

#[test]
fn terminal_source_protocol_capability_is_required_only_for_preparation() {
    // Causes: C1 Prepare/Dispose; C2 isolated/trusted host; C3 supervision
    // disabled/enabled. R1 Prepare+C2(any)+C3(any) -> require the decoder and
    // enforcer capability; R2 Dispose+C2(any)+C3(any) -> retain the prior wire
    // requirement. The existing process-owner test uses this same disposal
    // fixture and independently preserves every physical placement demand.
    use awaken_provisioning_contract::HostAccessPolicy;
    use awaken_session_contract::SessionTerminalCleanupAction;
    let disposal = terminal_disposal_action("source-protocol");
    for trusted in [false, true] {
        for supervised in [false, true] {
            let mut session = persisted("source-protocol", true, "terminated");
            let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
                &mut session.baseline
            else {
                panic!("frozen fixture");
            };
            baseline.environment.sandbox.host_access = if trusted {
                HostAccessPolicy::TrustedHost
            } else {
                HostAccessPolicy::Isolated
            };
            baseline.environment.sandbox.process_tree_supervision = supervised;
            for action in [
                &SessionTerminalCleanupAction::Prepare { commands: vec![] },
                &disposal,
            ] {
                let requirements =
                    awaken_session_realization_contract::terminal_cleanup_worker_requirements(
                        "workspace",
                        &session,
                        action,
                    )
                    .unwrap();
                assert_eq!(
                    requirements.required_capabilities.contains(
                        awaken_worker_contract::SESSION_TERMINAL_SOURCE_ADMISSION_CAPABILITY
                    ),
                    matches!(action, SessionTerminalCleanupAction::Prepare { .. }),
                    "trusted={trusted}, supervised={supervised}, action={action:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn terminal_source_authorization_uses_current_dispatch_without_business_closure() {
    // Causes: C1 a terminal root has its exact frozen scope, bound source and
    // live current lease; C2 current Dispatch is Settled/Pending/Unavailable;
    // C3 whole-execution recovery remains unproven. R1 C1+Settled+C3 -> live
    // source permission; R2 C1+(Pending|Unavailable)+C3 -> receipt-only
    // permission. Every row is a read: keep the entire Root and perform no
    // ordinary activation or physical cleanup. The existing real SQLite Host
    // regression separately derives Settled from an ordinary physical ACK.
    use awaken_session_execution_contract::SessionSourceDispatchObservation as Source;
    let session_id = "source-authorization";
    let lease = crate::session_resource::SessionRealizationLease {
        owner: "source-worker".into(),
        runtime_incarnation: "source-worker:1:boot".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    };
    let mut root = registry_race_terminal_session(session_id, Some(lease.clone()));
    root.environment = awaken_session_contract::SessionEnvironmentState::Resident {
        binding: serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
            "fixture",
            "original-source",
        ))
        .unwrap(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    let repo = sqlite_session_repository("terminal source authorization").await;
    create(repo.as_ref(), root).await;
    let before = repo.get(&SessionId::from(session_id)).await.unwrap();
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(matches!(
        awaken_session_execution_contract::SessionCoordinationReads::execution_recovery_observation(
            runtime.as_ref(),
            &before,
        )
        .await,
        awaken_session_execution_contract::SessionExecutionRecoveryObservation::NotProven { .. }
    ));
    let work = application
        .terminal_cleanups
        .terminal_cleanup_work(&SessionId::from(session_id), &lease)
        .await
        .unwrap()
        .unwrap();
    let effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        terminal_prepare_commands(work).into_iter().next().unwrap(),
        lease,
    );
    for source in [Source::Settled, Source::Pending, Source::default()] {
        let live = source == Source::Settled;
        *runtime.source_observation.lock().unwrap() = source;
        let authorization = application
            .terminal_cleanups
            .authorize_terminal_cleanup_effect(&effect)
            .await
            .unwrap();
        assert_eq!(authorization.permits_live_source(), live, "R1/R2");
        assert_eq!(
            repo.get(&SessionId::from(session_id)).await.unwrap(),
            before
        );
        assert_eq!(
            runtime.ordinary_resource_apply_calls.load(Ordering::SeqCst),
            0
        );
        assert!(runtime.intents.lock().unwrap().is_empty());
    }
}
