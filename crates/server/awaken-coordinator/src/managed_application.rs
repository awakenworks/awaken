//! Canonical process-local Managed Runtime/Application binding.

use std::sync::Arc;

use awaken_session_application::SessionApplication;
use awaken_session_executor::{
    AgentCoordinationInstallError, ExecutorComponents, SessionApplicationBindingError,
};

/// Connect the Runtime's fixed coordination tools to the one canonical Session
/// application. Product and scenario startup both use this helper so neither
/// can omit or independently encode the trait-object downgrade. `host`'s
/// Session application ports are its edges back into that application.
pub fn install_managed_agent_coordination(
    host: &ExecutorComponents,
    application: &Arc<SessionApplication>,
) -> Result<(), AgentCoordinationInstallError> {
    let dispatch = host.coordination().dispatch_observation_source()?;
    application
        .execution_admission()
        .verify_dispatch(&dispatch)
        .map_err(|_| AgentCoordinationInstallError::DispatchObservationMismatch)?;
    let coordination: Arc<dyn awaken_session_execution_contract::SessionAgentCoordination> =
        application.clone();
    let bindings = host.session_application_bindings();
    bindings
        .bind_agent_coordination(coordination)
        .map_err(|_| AgentCoordinationInstallError::AlreadyInstalled)?;
    bindings
        .require_agent_coordination()
        .map_err(|_| AgentCoordinationInstallError::ApplicationUnavailable)
}

/// Bind `host`'s sole committed-progress wakeup to the Session application's
/// lifecycle reconciliation. The wakeup holds the application weakly and only
/// asks it to reread durable truth.
pub(crate) fn bind_committed_progress_wakeup(
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
