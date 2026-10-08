//! Canonical process-local Managed Runtime/Application binding.

use std::sync::Arc;

use awaken_session_executor::{
    AgentCoordinationInstallError, CyclicPortAlreadyBound, ManagedHost, SharedHost,
};
use awaken_session_application::SessionApplication;

/// Connect the Runtime's fixed coordination tools to the one canonical Session
/// application. Product and scenario startup both use this helper so neither
/// can omit or independently encode the trait-object downgrade. `host`'s
/// Session application ports are its edges back into that application.
pub fn install_managed_agent_coordination(
    host: &SharedHost,
    runtime: &Arc<ManagedHost>,
    application: &Arc<SessionApplication>,
) -> Result<(), AgentCoordinationInstallError> {
    application
        .set_dispatch_observation_source(runtime.dispatch_observation_source()?)
        .map_err(|_| AgentCoordinationInstallError::DispatchObservationAlreadyInstalled)?;
    let coordination: Arc<dyn awaken_session_execution_contract::SessionAgentCoordination> =
        application.clone();
    host.session_application_ports()
        .agent_coordination
        .bind(Arc::downgrade(&coordination))
        .map_err(|_| AgentCoordinationInstallError::AlreadyInstalled)
}

/// Bind `host`'s sole committed-progress wakeup to the Session application's
/// lifecycle reconciliation. The wakeup holds the application weakly and only
/// asks it to reread durable truth.
pub(crate) fn bind_committed_progress_wakeup(
    host: &SharedHost,
    application: &Arc<SessionApplication>,
) -> Result<(), CyclicPortAlreadyBound> {
    let application = Arc::downgrade(application);
    host.session_application_ports()
        .committed_progress
        .bind(Arc::new(move || {
            if let Some(application) = application.upgrade() {
                application.request_lifecycle_reconciliation();
            }
        }))
}
