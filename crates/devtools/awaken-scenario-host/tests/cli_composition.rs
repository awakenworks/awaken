//! Cross-component CLI composition belongs to the existing scenario test root.
use std::sync::Arc;

use awaken_agent_contract::ThreadId;
use awaken_cli::{CoordinatorExecutionError, coordinator_execution};
use awaken_dispatch::ClaimedOperationCoordinatorResolver;
use awaken_process_startup::SessionCommitOwner;
use awaken_session_executor::AgentCoordinationInstallError;

#[tokio::test]
async fn composition_selects_the_executor_components_and_binds_once() {
    // C1: the scenario root composes through `coordinator_execution`; its
    // ports are the executor's own dispatch, coordinator dispatch and
    // storage root. C2: a second composition over the same executor and
    // application refuses before selecting a second set of ports.
    let resources = awaken_resource_persistence::ephemeral(
        awaken_time::system_clock(),
        Arc::new(awaken_resource_contract::LocalInvalidator::default()),
    )
    .await
    .expect("ephemeral Resources");
    let dependencies = awaken_session_executor_testkit::volatile_dependencies(
        Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
        "stub",
    )
    .await;
    let assembled = awaken_scenario_host::local_managed_state(
        dependencies,
        resources.authorities().resource_registry(),
    )
    .await;
    let ports = &assembled.state.execution;
    assert!(
        Arc::ptr_eq(&ports.dispatch, assembled.host.dispatch()),
        "C1"
    );
    assert!(
        Arc::ptr_eq(
            &ports.coordinator_dispatch,
            &assembled
                .host
                .session_runs()
                .coordinator_dispatch()
                .expect("Coordinator dispatch"),
        ),
        "C1"
    );
    assert_eq!(
        ports.storage_dir.as_deref(),
        assembled.host.storage_dir(),
        "C1"
    );
    assert!(
        matches!(
            coordinator_execution(&assembled.host, &assembled.state.sessions),
            Err(CoordinatorExecutionError::Coordination(
                AgentCoordinationInstallError::AlreadyInstalled
            ))
        ),
        "C2"
    );
    // C3: an application admitted over another executor's dispatch is
    // refused before any edge of this executor is touched.
    let other = awaken_scenario_host::local_managed_state(
        awaken_session_executor_testkit::volatile_dependencies(
            Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
            "stub",
        )
        .await,
        resources.authorities().resource_registry(),
    )
    .await;
    assert!(
        matches!(
            coordinator_execution(&assembled.host, &other.state.sessions),
            Err(CoordinatorExecutionError::Coordination(
                AgentCoordinationInstallError::DispatchObservationMismatch
            ))
        ),
        "C3"
    );
}

#[tokio::test]
async fn composition_rejects_started_dispatch_before_binding_applications() {
    // C4 decision table: local pool OR remote terminal loop already chose
    // a raw sink => DispatchAlreadyStarted, no coordination/background
    // binding written. Both cases use the real queue and real service.
    // C1-C3 above still own cold composition, replay and foreign dispatch.
    for remote in [false, true] {
        let clock = awaken_time::system_clock();
        let queue = Arc::new(awaken_dispatch::MemoryDispatchStore::new(clock.clone()));
        let owner =
            Arc::new(awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_dispatch(queue));
        let dependencies = awaken_session_executor_testkit::volatile_dependencies(
            Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
            "stub",
        )
        .await;
        let dependencies = if remote {
            dependencies.with_ephemeral_coordinator_dispatch(owner)
        } else {
            dependencies.with_ephemeral_dispatch(owner)
        };
        let host = dependencies.build().unwrap();
        let application = Arc::new(
            awaken_session_application::SessionApplicationBuilder::new(
                host.session_execution_roles(),
                awaken_session_application::SessionPersistence::new(
                    awaken_protocol_managed_testkit::ephemeral_session_repository().await,
                    clock.clone(),
                ),
                awaken_protocol_managed_testkit::environment_fixture(clock).execution,
                {
                    let probe = std::sync::Arc::new(
                        awaken_session_application::ExecutionObservationProbe::default(),
                    );
                    awaken_session_application::SessionExecutionAdmission::new(
                        host.coordination().dispatch_observation_source().unwrap(),
                        probe.clone(),
                        probe,
                    )
                },
                Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            )
            .build()
            .unwrap(),
        );
        let roles = awaken_session_executor::session_dispatch_lifecycle(
            host.as_ref(),
            host.worker_completion_sink(),
            awaken_session_execution_contract::SessionApplications::Standalone,
        )
        .unwrap();
        if remote {
            assert!(roles.start_terminal_reconciliation(), "C4 scanner started");
        } else {
            roles.start_local_pool().unwrap();
            assert!(host.dispatch().pool().is_some(), "C4 pool started");
        }
        assert!(
            matches!(
                coordinator_execution(&host, &application),
                Err(CoordinatorExecutionError::DispatchAlreadyStarted)
            ),
            "C4 immutable completion selection"
        );
        let binding = host.session_application_binding();
        assert_eq!(
            binding.mode(),
            Ok(awaken_session_execution_contract::SessionApplicationMode::Standalone),
            "C4 neither application role installed and original mode unchanged"
        );
        assert!(
            roles.drain(std::time::Duration::from_secs(2)).await,
            "C4 owned service drains"
        );
    }
}

#[tokio::test]
async fn shared_binding_preserves_host_owner_selection() {
    // Causes: C1 one selected committed-read owner, C2 a non-resident
    // Session Thread, C3 its execution composition is released. R1 C1+C2:
    // the binding selects the exact read owner, resolves only its configured
    // commit boundary and never installs a resident Session. R2 C1+C2+C3:
    // the binding releases the composition and still resolves through the
    // same read owner. Claim/epoch/partition/replay decisions remain owned
    // by the existing commit_ingest tests; this adapter adds no such rule.
    let host = awaken_session_executor_testkit::volatile_host(
        Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
        "stub",
    )
    .await;
    let thread = ThreadId("unknown-shared-commit-session".into());
    let reads = host.thread_reads().clone();
    let binding = SessionCommitOwner(reads.clone());
    assert!(Arc::ptr_eq(&binding.0, &reads), "R1 exact read owner");
    assert!(
        host.capabilities()
            .registered_thread_workspace(&thread.0)
            .expect("healthy registry")
            .is_none()
    );
    host.thread_reads()
        .commit_owner(&thread)
        .await
        .expect("configured Host commit boundary");
    binding
        .resolve(&thread)
        .await
        .expect("shared binding reuses the Host boundary");
    assert!(
        host.capabilities()
            .registered_thread_workspace(&thread.0)
            .expect("healthy registry")
            .is_none()
    );
    let composition = Arc::downgrade(&host);
    drop(host);
    assert!(composition.upgrade().is_none(), "R2 no retained executor");
    binding
        .resolve(&thread)
        .await
        .expect("R2 original configured commit source survives composition release");
    assert!(composition.upgrade().is_none(), "R2 no resurrection");
}
