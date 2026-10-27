// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: Composition root that binds the Session executor into the Session application and
// @formal reason: selects the Coordinator's execution ports; classified against its product
// @formal reason: requirement, this link is not a formal-proof claim.
//! The launcher-owned co-located composition over its Session
//! executor.
//!
//! The Coordinator component links no executor: it consumes
//! [`awaken_coordinator::ExecutionPorts`]. This root, which builds the
//! executor, binds its co-located edges back into the Session application and
//! selects those ports from it. The local-placement branch and scenario mounts
//! compose through [`coordinator_execution`]. Registered-Worker production uses
//! the shared delivery/commit owners in `runtime_process_router::registered_execution`
//! directly; both compositions use this module's authority/startup guard.

use std::sync::Arc;

#[cfg(test)]
use awaken_agent_contract::agent::thread::Id as ThreadId;

#[cfg(test)]
use awaken_dispatch::ClaimedOperationCoordinatorResolver;
use awaken_process_startup::SessionCommitOwner;
use awaken_session_application::{SessionApplication, SessionRunApplication};
use awaken_session_application_contract::{RunApplication, SessionRunBackgroundApplication};

use awaken_session_executor::{
    AgentCoordinationInstallError, ExecutorComponents, SessionApplicationBindingError,
};

/// Why the Coordinator could not be composed over this executor.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorExecutionError {
    #[error("install Session Agent coordination: {0}")]
    Coordination(#[from] AgentCoordinationInstallError),
    #[error("bind the Session application into the executor: {0}")]
    Binding(#[from] SessionApplicationBindingError),
    #[error("select the Coordinator dispatch authority: {0}")]
    Dispatch(String),
    #[error("compose Session execution before starting dispatch services")]
    DispatchAlreadyStarted,
}

/// Bind the executor's edges back into the one Session application and
/// select the Coordinator's ports from the executor.
///
/// Decision table (each row is one composition outcome):
/// | Rule | Session application | Executor bindings | Effect |
/// |---|---|---|---|
/// | C1 | observes the executor's dispatch | unbound | edges bound; ports select the executor's own components |
/// | C2 | observes the executor's unstarted dispatch | already bound by an earlier composition | `Coordination(AlreadyInstalled)`, no second ports |
/// | C3 | observes another dispatch | - | `Coordination(DispatchObservationMismatch)`, nothing bound |
///
/// C4: an already-started pool or terminal scanner is refused before either
/// application binding changes. Its fixed completion port cannot be upgraded
/// after work starts; compose the application before starting these services.
///
/// The executor's own binding and dispatch-ownership refusals reach the root
/// unchanged through `Binding` and `Dispatch`; their rules are the executor's.
pub fn coordinator_execution(
    host: &Arc<ExecutorComponents>,
    sessions: &Arc<SessionApplication>,
) -> Result<awaken_coordinator::ExecutionPorts, CoordinatorExecutionError> {
    let dispatch = host.coordination().dispatch_observation_source()?;
    verify_composition(host.dispatch(), &dispatch, sessions)?;
    install_managed_agent_coordination(host, sessions)?;
    let runs: Arc<dyn RunApplication> = host.session_runs().clone();
    let workspace_host = host.clone();
    let agent_host = host.clone();
    let session_runs = Arc::new(SessionRunApplication::new(
        runs,
        sessions.run_admissions(),
        sessions.coordination(),
        move |thread| Ok(workspace_host.capabilities().thread_workspace(thread)?),
        move |thread| Ok(agent_host.capabilities().thread_agent_projection(thread)?),
    ));
    let background: Arc<dyn SessionRunBackgroundApplication> = session_runs.clone();
    let bindings = host.session_application_bindings();
    bindings.bind_background_run_submission(background)?;
    bindings.require_background_run_submission()?;
    let coordinator_dispatch = host
        .session_runs()
        .coordinator_dispatch()
        .map_err(|error| CoordinatorExecutionError::Dispatch(error.to_string()))?;
    let completion = Arc::new(awaken_session_run_delivery::SettlementWakeup::new(
        host.worker_completion_sink(),
        sessions.lifecycle().reconciliation_wakeup(),
    ));
    let roles =
        awaken_session_executor::session_dispatch_lifecycle(host.as_ref(), completion.clone());
    Ok(awaken_coordinator::ExecutionPorts {
        session_runs,
        durable_runs: host.session_runs().clone(),
        dispatch: roles.dispatch().clone(),
        dispatch_lifecycle: Arc::new(roles),
        coordinator_dispatch,
        commit_owners: Arc::new(SessionCommitOwner(host.thread_reads().clone())),
        worker_recovery: host.worker_recovery_source(),
        worker_completion: completion,
        worker_settlement: host.worker_memory_settlement_observer(),
        worker_stream: host.worker_stream_sink(),
        worker_file_content: host.worker_file_content_source(),
        storage_dir: host.storage_dir().map(std::path::Path::to_path_buf),
    })
}

fn verify_composition(
    process: &awaken_dispatch_execution::ProcessDispatch,
    dispatch: &Arc<dyn awaken_dispatch::DispatchOperationalFeed>,
    sessions: &SessionApplication,
) -> Result<(), CoordinatorExecutionError> {
    awaken_process_startup::verify_composition(process, dispatch, sessions).map_err(|error| {
        match error {
            awaken_process_startup::ExecutionCompositionError::DispatchObservationMismatch => {
                CoordinatorExecutionError::Coordination(
                    AgentCoordinationInstallError::DispatchObservationMismatch,
                )
            }
            awaken_process_startup::ExecutionCompositionError::DispatchAlreadyStarted => {
                CoordinatorExecutionError::DispatchAlreadyStarted
            }
        }
    })
}

/// Connect the executor's fixed coordination tools to the one canonical
/// Session application, after verifying both observe the same dispatch.
fn install_managed_agent_coordination(
    host: &ExecutorComponents,
    application: &Arc<SessionApplication>,
) -> Result<(), AgentCoordinationInstallError> {
    let coordination: Arc<dyn awaken_session_execution_contract::SessionAgentCoordination> =
        application.coordination();
    let bindings = host.session_application_bindings();
    bindings
        .bind_agent_coordination(coordination)
        .map_err(|_| AgentCoordinationInstallError::AlreadyInstalled)?;
    bindings
        .require_agent_coordination()
        .map_err(|_| AgentCoordinationInstallError::ApplicationUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let owner = Arc::new(
                awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_dispatch(queue),
            );
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
                    awaken_session_application::SessionExecutionAdmission::fixture_with_dispatch(
                        host.coordination().dispatch_observation_source().unwrap(),
                    ),
                    Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
                )
                .build()
                .unwrap(),
            );
            let roles = awaken_session_executor::session_dispatch_lifecycle(
                host.as_ref(),
                host.worker_completion_sink(),
            );
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
            let bindings = host.session_application_bindings();
            assert!(
                bindings.require_agent_coordination().is_err(),
                "C4 no coordination bound"
            );
            assert!(
                bindings.require_background_run_submission().is_err(),
                "C4 no background bound"
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
}
