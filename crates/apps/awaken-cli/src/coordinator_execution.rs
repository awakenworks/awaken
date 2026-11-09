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

use awaken_runtime_contract::runs::Runs;
use awaken_session_application::{SessionApplication, SessionRunApplication};
use awaken_session_application_contract::SessionRunBackgroundApplication;
use awaken_session_execution_contract::{SessionApplicationBindingError, SessionApplications};

use awaken_session_executor::{AgentCoordinationInstallError, ExecutorComponents};

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
    let runs: Arc<dyn Runs> = host.session_runs().clone();
    let (workspace, projected_agent) = host.capabilities().session_run_projection_callbacks();
    let session_runs = Arc::new(SessionRunApplication::new(
        runs,
        sessions.run_admissions(),
        sessions.coordination(),
        workspace,
        projected_agent,
    ));
    let background: Arc<dyn SessionRunBackgroundApplication> = session_runs.clone();
    let coordinator_dispatch = host
        .session_runs()
        .coordinator_dispatch()
        .map_err(|error| CoordinatorExecutionError::Dispatch(error.to_string()))?;
    let worker_commits =
        awaken_session_executor::coordinator_run_commits(host).ok_or_else(|| {
            CoordinatorExecutionError::Dispatch("executor has no local commit authority".to_owned())
        })?;
    let completion = Arc::new(awaken_session_run_delivery::SettlementWakeup::new(
        host.worker_completion_sink(),
        sessions.lifecycle().reconciliation_wakeup(),
    ));
    let roles = awaken_session_executor::session_dispatch_lifecycle(
        host.as_ref(),
        completion.clone(),
        SessionApplications::Local {
            coordination: sessions.coordination(),
            background_runs: background,
        },
    )
    .map_err(|error| match error {
        SessionApplicationBindingError::AlreadyBound => {
            CoordinatorExecutionError::Coordination(AgentCoordinationInstallError::AlreadyInstalled)
        }
        SessionApplicationBindingError::ServicesAlreadyStarted => {
            CoordinatorExecutionError::DispatchAlreadyStarted
        }
        error => CoordinatorExecutionError::Binding(error),
    })?;
    Ok(awaken_coordinator::ExecutionPorts {
        session_runs,
        durable_runs: host.session_runs().clone(),
        dispatch: roles.dispatch().clone(),
        dispatch_lifecycle: Arc::new(roles),
        worker_commits,
        coordinator_dispatch,
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
