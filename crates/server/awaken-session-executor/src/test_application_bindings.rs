// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-executor/src/process_roles.rs
// @formal reason: cfg(test) only; strict unused-role ports for existing mechanism fixtures. Production startup tests use the public complete constructor.
use awaken_agent_contract::agent::{message::Message, run::Id as RunId, thread::Id as ThreadId};
use awaken_session_application_contract::{RunApplicationError, SessionRunBackgroundApplication};
use awaken_session_execution_contract::*;
use awaken_session_types::{RunError, SessionId};
use std::sync::{Arc, OnceLock};

pub(crate) struct UnusedApplications {
    _host: Option<Arc<crate::ExecutorComponents>>,
}
pub(crate) fn fresh() -> Arc<UnusedApplications> {
    Arc::new(UnusedApplications { _host: None })
}
pub(crate) fn retaining(host: Arc<crate::ExecutorComponents>) -> Arc<UnusedApplications> {
    Arc::new(UnusedApplications { _host: Some(host) })
}
#[async_trait::async_trait]
impl SessionAgentCoordination for UnusedApplications {
    // This complete-startup fixture owns lifetimes only. B's prepared callbacks
    // must remain strict unused roles, never fake a reservation/activity grant.
    async fn prepare_prepared_session_run_activity(
        &self,
        _: &SessionId,
        _: &str,
        _: &RunId,
        _: SessionRunActivityAdmissionMode,
    ) -> Result<Option<awaken_session_types::SessionRevision>, RunError> {
        panic!("unused prepared coordination projection role was invoked")
    }
    async fn admit_prepared_session_run_activity(
        &self,
        _: &SessionId,
        _: &RunId,
        _: SessionRunActivityAdmissionMode,
        _: PreparedSessionRunActivity,
    ) -> Result<SessionRunActivityAdmission, RunError> {
        panic!("unused prepared coordination activity role was invoked")
    }
    async fn admit_session_run_activity(
        &self,
        _: &SessionId,
        _: &str,
        _: &RunId,
        _: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, RunError> {
        panic!("unused coordination activity role was invoked")
    }
    async fn admit_session_model_request(
        &self,
        _: &SessionId,
        _: &ThreadId,
        _: &RunId,
    ) -> Result<bool, RunError> {
        panic!("unused model admission role was invoked")
    }
    async fn list_session_agents(
        &self,
        _: &SessionId,
    ) -> Result<Vec<SessionAgentRosterEntry>, RunError> {
        panic!("unused coordination roster role was invoked")
    }
    async fn send_session_agent_message(
        &self,
        _: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, RunError> {
        panic!("unused coordination send role was invoked")
    }
    async fn settle_session_agent_boundary(
        &self,
        _: SessionAgentBoundaryCommand,
    ) -> Result<(), RunError> {
        panic!("unused coordination settle role was invoked")
    }
    async fn finalize_session_agent_boundary(
        &self,
        _: SessionAgentBoundaryCommand,
    ) -> Result<(), RunError> {
        panic!("unused coordination finalize role was invoked")
    }
    async fn interrupt_session_thread(&self, _: &SessionId, _: &ThreadId) -> Result<(), RunError> {
        panic!("unused coordination interrupt role was invoked")
    }
    async fn reply_session_thread_tool(
        &self,
        _: SessionThreadToolReplyCommand,
    ) -> Result<(), RunError> {
        panic!("unused coordination reply role was invoked")
    }
}
#[async_trait::async_trait]
impl SessionRunBackgroundApplication for UnusedApplications {
    async fn submit_session_run_background(
        &self,
        _: &str,
        _: &str,
        _: Option<String>,
        _: Vec<Message>,
        _: Option<String>,
    ) -> Result<RunId, RunApplicationError> {
        panic!("unused background submission role was invoked")
    }
}
fn unused() -> Arc<UnusedApplications> {
    static PORTS: OnceLock<Arc<UnusedApplications>> = OnceLock::new();
    PORTS.get_or_init(fresh).clone()
}
pub(crate) fn background() -> Arc<dyn SessionRunBackgroundApplication> {
    unused()
}
pub(crate) fn coordination() -> Arc<dyn SessionAgentCoordination> {
    unused()
}

/// Select ordinary Thread execution explicitly for a lower-level fixture.
/// No Session application is silently installed, and production preconditions
/// remain owned by the same public constructor used in startup tests.
pub(crate) fn select_standalone(
    host: &crate::ExecutorComponents,
) -> Result<(), crate::SessionApplicationBindingError> {
    drop(crate::session_dispatch_lifecycle(
        host,
        host.worker_completion_sink(),
        awaken_session_run_delivery::SessionApplications::Standalone,
    )?);
    Ok(())
}

// Weak-only setup for legacy mechanism fixtures: callers retain their tested
// application; the complementary unused role is process-static and panics on
// use. Ownership/startup sign-off always uses the production constructor.
pub(crate) fn select_coordination(
    host: &crate::ExecutorComponents,
    application: Arc<dyn SessionAgentCoordination>,
) -> Result<(), crate::SessionApplicationBindingError> {
    let (connections, lifetime) = awaken_session_run_delivery::SessionApplications::Local {
        coordination: application,
        background_runs: background(),
    }
    .connect();
    host.session_application_bindings()
        .select_connections(connections)?;
    drop(lifetime);
    Ok(())
}
pub(crate) fn select_background(
    host: &crate::ExecutorComponents,
    application: Arc<dyn SessionRunBackgroundApplication>,
) -> Result<(), crate::SessionApplicationBindingError> {
    let (connections, lifetime) = awaken_session_run_delivery::SessionApplications::Local {
        coordination: coordination(),
        background_runs: application,
    }
    .connect();
    host.session_application_bindings()
        .select_connections(connections)?;
    drop(lifetime);
    Ok(())
}

/// Existing lower-level fixtures may reuse their already-selected complete
/// applications. New production readiness tests never use this helper.
pub(crate) fn lifecycle(
    host: &crate::ExecutorComponents,
    completion: Arc<dyn awaken_dispatch_contract::CompletionSink>,
) -> awaken_session_run_delivery::SessionDispatchLifecycle<
    awaken_session_run_delivery::SessionApplicationLifetime,
> {
    if host.session_application_bindings().mode().is_some() {
        crate::process_roles::selected_test_lifecycle(host, completion)
            .expect("fixture must retain both complete local roles")
    } else {
        let selected = if host.run_assembly.authority.hosts_session_application() {
            awaken_session_run_delivery::SessionApplications::Standalone
        } else {
            awaken_session_run_delivery::SessionApplications::ClaimedWorker
        };
        crate::session_dispatch_lifecycle(host, completion, selected)
            .expect("explicit complete fixture topology")
    }
}
