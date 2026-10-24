// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: Composition root that binds the Session executor into the Session application and
// @formal reason: selects the Coordinator's execution ports; classified against its product
// @formal reason: requirement, this link is not a formal-proof claim.
//! The co-located composition of the Coordinator over this process's Session
//! executor.
//!
//! The Coordinator component links no executor: it consumes
//! [`awaken_coordinator::ExecutionPorts`]. This root, which builds the
//! executor, is the one place that binds the executor's edges back into the
//! Session application and selects those ports from it. Every process that
//! is both Coordinator and Worker (this binary, the scenario host, tests)
//! composes through [`coordinator_execution`].

use std::sync::Arc;

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator;
use awaken_dispatch::ClaimedOperationCoordinatorResolver;
use awaken_session_application::{SessionApplication, SessionRunApplication};
use awaken_session_application_contract::{RunApplication, SessionRunBackgroundApplication};
use awaken_session_execution_contract::ApplicationError;
use awaken_session_executor::{
    AgentCoordinationInstallError, CoordinatorRoles, ExecutorComponents,
    SessionApplicationBindingError,
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
}

/// Bind the executor's edges back into the one Session application and
/// select the Coordinator's ports from the executor.
///
/// Decision table (each row is one composition outcome):
/// | Rule | Session application | Executor bindings | Effect |
/// |---|---|---|---|
/// | C1 | observes the executor's dispatch | unbound | edges bound; ports select the executor's own components |
/// | C2 | observes the executor's dispatch | already bound by an earlier composition | `Coordination(AlreadyInstalled)`, no second ports |
/// | C3 | observes another dispatch | - | `Coordination(DispatchObservationMismatch)`, nothing bound |
///
/// The executor's own binding and dispatch-ownership refusals reach the root
/// unchanged through `Binding` and `Dispatch`; their rules are the executor's.
pub fn coordinator_execution(
    host: &Arc<ExecutorComponents>,
    sessions: &Arc<SessionApplication>,
) -> Result<awaken_coordinator::ExecutionPorts, CoordinatorExecutionError> {
    install_managed_agent_coordination(host, sessions)?;
    bind_committed_progress_wakeup(host, sessions)?;
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
    let roles = CoordinatorRoles::of(host);
    Ok(awaken_coordinator::ExecutionPorts {
        session_runs,
        durable_runs: host.session_runs().clone(),
        dispatch: roles.dispatch().clone(),
        dispatch_lifecycle: Arc::new(roles),
        coordinator_dispatch,
        commit_owners: Arc::new(SessionCommitOwner(host.clone())),
        worker_recovery: host.worker_recovery_source(),
        worker_completion: host.worker_completion_sink(),
        worker_settlement: host.worker_memory_settlement_observer(),
        worker_stream: host.worker_stream_sink(),
        worker_file_content: host.worker_file_content_source(),
        storage_dir: host.storage_dir().map(std::path::Path::to_path_buf),
    })
}

/// Connect the executor's fixed coordination tools to the one canonical
/// Session application, after verifying both observe the same dispatch.
fn install_managed_agent_coordination(
    host: &ExecutorComponents,
    application: &Arc<SessionApplication>,
) -> Result<(), AgentCoordinationInstallError> {
    let dispatch = host.coordination().dispatch_observation_source()?;
    application
        .execution_admission()
        .verify_dispatch(&dispatch)
        .map_err(|_| AgentCoordinationInstallError::DispatchObservationMismatch)?;
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

/// Bind the executor's sole committed-progress wakeup to the Session
/// application's lifecycle reconciliation. The wakeup holds the application
/// weakly and only asks it to reread durable truth.
fn bind_committed_progress_wakeup(
    host: &ExecutorComponents,
    application: &Arc<SessionApplication>,
) -> Result<(), SessionApplicationBindingError> {
    let application = Arc::downgrade(application);
    host.session_application_bindings()
        .bind_lifecycle_reconciliation_wakeup(Arc::new(move || {
            if let Some(application) = application.upgrade() {
                application.lifecycle().request_reconciliation();
            }
        }))
}

/// Binds [`ClaimedOperationCoordinatorResolver`] to
/// [`awaken_session_executor::session_commit_owner`]. The executor only
/// resolves the existing owner; `ClaimedCommitService` remains the sole
/// caller of its write method.
struct SessionCommitOwner(Arc<ExecutorComponents>);

#[async_trait::async_trait]
impl ClaimedOperationCoordinatorResolver for SessionCommitOwner {
    async fn resolve(
        &self,
        session_thread_id: &ThreadId,
    ) -> Result<Arc<dyn OperationCoordinator>, ApplicationError> {
        awaken_session_executor::session_commit_owner(&self.0, session_thread_id).await
    }
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
    async fn shared_binding_preserves_host_owner_selection() {
        // Causes: one selected Host and a non-resident Session Thread. Rule R1:
        // the shared binding retains that exact Host and resolves its configured
        // commit boundary without installing a resident Session. Existing
        // commit_ingest tests own valid/stale claims,
        // child partitioning, replay and persistence. This test covers only
        // the newly shared binding, not a second implementation of those rules.
        let host = awaken_session_executor_testkit::volatile_host(
            Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
            "stub",
        )
        .await;
        let thread = ThreadId("unknown-shared-commit-session".into());
        let binding = SessionCommitOwner(host.clone());
        assert!(Arc::ptr_eq(&binding.0, &host));
        assert!(
            host.capabilities()
                .registered_thread_workspace(&thread.0)
                .expect("healthy registry")
                .is_none()
        );
        awaken_session_executor::session_commit_owner(&host, &thread)
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
    }
}
