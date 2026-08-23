//! Canonical process-local Managed Runtime/Application binding.

use std::sync::Arc;

use awaken_runtime_host::{AgentCoordinationInstallError, ManagedHost};
use awaken_session_application::SessionApplication;

/// Connect the Runtime's fixed coordination tools to the one canonical Session
/// application. Product and scenario startup both use this helper so neither
/// can omit or independently encode the trait-object downgrade.
pub fn install_managed_agent_coordination(
    runtime: &Arc<ManagedHost>,
    application: &Arc<SessionApplication>,
) -> Result<(), AgentCoordinationInstallError> {
    let coordination: Arc<dyn awaken_session_contract::SessionAgentCoordination> =
        application.clone();
    runtime.install_agent_coordination_application(Arc::downgrade(&coordination))
}
