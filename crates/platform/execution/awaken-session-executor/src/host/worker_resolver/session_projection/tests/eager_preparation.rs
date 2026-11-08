use super::*;
use crate::host::worker_resolver::test_support::empty_frozen_projection_for_snapshot;

#[tokio::test]
async fn run_free_preparation_preserves_lazy_and_remote_environment_boundaries() {
    for remote in [false, true] {
        let storage = tempfile::tempdir().expect("owned storage");
        let thread = if remote {
            "remote-preparation"
        } else {
            "lazy-preparation"
        };
        let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await)
            .build()
            .expect("Host dependencies");
        let snapshot = if remote {
            awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
                .model(
                    awaken_runtime_contract::resolved::ModelBinding::parse(
                        "remote-agent",
                        "",
                        "a2a:https://agent.example.test",
                    )
                    .expect("valid remote fixture binding"),
                )
                .build()
        } else {
            test_activation(thread, "unused-model-run").snapshot
        };
        let mut environment = eager_environment();
        if !remote {
            environment.sandbox_provisioning =
                awaken_provisioning_contract::SandboxProvisioning::OnToolUse;
        }
        let mut projection =
            empty_frozen_projection_for_snapshot("workspace", environment, &snapshot);
        projection.agent_publication = Some(snapshot);
        let control = RecoveryControl::fixed(projection.clone());
        let sink = Arc::new(RecoveryEnvironmentBindingSink::new(&control));
        host.run_assembly
            .install_test_environment_binding_sink(sink.clone());
        let directive = awaken_session_execution_contract::SessionRealizationDirective {
            projection,
            authority: awaken_session_realization::test_support::test_lease_receipt(
                awaken_session_configuration_contract::resource::SessionRealizationLease {
                    owner: "worker-a".into(),
                    runtime_incarnation: "worker-a".into(),
                    epoch: 1,
                    expires_at_unix_ms: u64::MAX,
                },
            ),
            action: awaken_session_execution_contract::SessionRealizationAction::Stage {
                prepare_session: true,
                mcp_stages: vec![],
            },
        };
        crate::application::worker_projection::realize_session(
            &host.run_assembly,
            &control,
            &awaken_session_types::SessionId::from(thread),
            directive,
            None,
            None,
            false,
        )
        .await
        .expect("preparation without a local Eager requirement");
        assert_eq!(
            sink.calls(),
            0,
            "lazy and remote-only Sessions create no local substrate"
        );
        assert!(control.current_projection().environment.binding().is_none());
    }
}

#[tokio::test]
async fn run_free_eager_preparation_materializes_once_without_a_model_run_or_stdio_trigger() {
    // C1 a Stage explicitly prepares an Eager Session; C2 its exact publication
    // is frozen; C3 there is neither a Run claim nor stdio MCP to trigger a
    // substrate indirectly. E1 materialize through the ordinary binding sink,
    // E2 retain only its resident substrate, E3 replay adopts without another
    // create. The former path returned Ok with Unmaterialized still persisted.
    let storage = tempfile::tempdir().expect("owned storage");
    let thread = "run-free-eager";
    let activation = test_activation(thread, "unused-model-run");
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .with_store_dir(storage.path())
        .await)
        .build()
        .expect("Host dependencies");
    let mut projection = frozen_projection();
    projection.agent_publication = Some(activation.snapshot);
    let control = RecoveryControl::fixed(projection.clone());
    let sink = Arc::new(RecoveryEnvironmentBindingSink::new(&control));
    host.run_assembly
        .install_test_environment_binding_sink(sink.clone());
    let authority = awaken_session_realization::test_support::test_lease_receipt(
        awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        },
    );
    for replay in [false, true] {
        let directive = awaken_session_execution_contract::SessionRealizationDirective {
            projection: if replay {
                control.current_projection()
            } else {
                projection.clone()
            },
            authority: authority.clone(),
            action: awaken_session_execution_contract::SessionRealizationAction::Stage {
                prepare_session: true,
                mcp_stages: vec![],
            },
        };
        crate::application::worker_projection::realize_session(
            &host.run_assembly,
            &control,
            &awaken_session_types::SessionId::from(thread),
            directive,
            None,
            None,
            false,
        )
        .await
        .expect("run-free Eager preparation");
        assert_eq!(
            sink.calls(),
            1,
            "the same committed binding is adopted on replay"
        );
        assert!(control.current_projection().environment.binding().is_some());
        assert!(
            host.run_assembly
                .installed_sessions
                .read(thread, |installed| installed.run.context.is_none()
                    && installed.realization.environment().owner.is_resident())
                .expect("installed Session")
                .unwrap_or(false),
            "prepare a substrate, never a model Run"
        );
    }
}

#[tokio::test]
async fn lease_only_projection_does_not_create_an_unmaterialized_eager_environment() {
    // An Eager baseline alone is not physical-effect authority. A lease-only
    // Complete projection retains the ordinary no-create behavior even when it
    // carries a valid publication; only a preparation Stage may initiate it.
    let storage = tempfile::tempdir().expect("owned storage");
    let thread = "eager-lease-only";
    let activation = test_activation(thread, "unused-model-run");
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .with_store_dir(storage.path())
        .await)
        .build()
        .expect("Host dependencies");
    let mut projection = frozen_projection();
    projection.agent_publication = Some(activation.snapshot);
    let control = RecoveryControl::fixed(projection.clone());
    let sink = Arc::new(RecoveryEnvironmentBindingSink::new(&control));
    host.run_assembly
        .install_test_environment_binding_sink(sink.clone());
    let directive = awaken_session_execution_contract::SessionRealizationDirective {
        projection,
        authority: awaken_session_realization::test_support::test_lease_receipt(
            awaken_session_configuration_contract::resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a".into(),
                epoch: 1,
                expires_at_unix_ms: u64::MAX,
            },
        ),
        action: awaken_session_execution_contract::SessionRealizationAction::Complete,
    };
    crate::application::worker_projection::realize_session(
        &host.run_assembly,
        &control,
        &awaken_session_types::SessionId::from(thread),
        directive,
        None,
        None,
        false,
    )
    .await
    .expect("lease-only projection");
    assert_eq!(
        sink.calls(),
        0,
        "a renewal never creates a physical Environment"
    );
    assert!(control.current_projection().environment.binding().is_none());
}

#[tokio::test]
async fn refused_run_free_preparation_closes_its_local_lease() {
    // C1 a Worker-claimed run-free preparation (no Run, no model claim);
    // C2 Control refuses the Activate after Stage, as `Conflict` does while an
    // interactive turn settles the same root (this fixture answers NotReady).
    // E1 preparation reports pending; E2 its local lease projection is closed,
    // so renewal stops and the durable lease expires for a later sweep instead
    // of keeping an unfinished preparation live forever.
    let storage = tempfile::tempdir().expect("owned storage");
    let thread = "refused-run-free-preparation";
    let control = Arc::new(crate::host::tests::fixtures::RemoteTerminalCleanupControl::default());
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .with_store_dir(storage.path())
        .await
        .with_session_realization_control(control.clone()))
    .build()
    .expect("Host dependencies");
    let activation = test_activation(thread, "unused-model-run");
    let mut environment = eager_environment();
    environment.sandbox_provisioning = awaken_provisioning_contract::SandboxProvisioning::OnToolUse;
    let mut projection =
        empty_frozen_projection_for_snapshot("workspace", environment, &activation.snapshot);
    projection.agent_publication = Some(activation.snapshot);
    let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    };
    let assignment = awaken_session_execution_contract::SessionPreparationAssignment {
        session_id: awaken_session_types::SessionId::from(thread),
        directive: awaken_session_execution_contract::SessionRealizationDirective {
            projection,
            authority: awaken_session_realization::test_support::test_lease_receipt(lease),
            action: awaken_session_execution_contract::SessionRealizationAction::Stage {
                prepare_session: false,
                mcp_stages: vec![],
            },
        },
    };
    host.run_assembly
        .prepare_session_without_run(&assignment)
        .await
        .expect_err("E1 a refused Activate leaves preparation pending");
    let live = host
        .run_assembly
        .installed_sessions
        .read(thread, |installed| {
            installed
                .realization
                .execution()
                .authority
                .lease()
                .is_some()
        })
        .expect("installed Sessions")
        .unwrap_or(false);
    assert!(
        !live,
        "E2 the refused run-free lease is no longer renewed locally"
    );
}
