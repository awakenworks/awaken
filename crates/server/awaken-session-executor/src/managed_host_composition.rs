// @formal entrypoint session_resources.install_replacement_requires_newer_or_unattempted_authority_amendment
// @formal entrypoint session_run.reservation_never_executes
// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
//! Process-local composition of the Managed adapter around one shared Host.

use super::*;

impl ManagedHost {
    /// Read side of this Host's sole durable dispatch authority. Composition
    /// supplies it to the Session application without exposing a second queue.
    pub fn dispatch_observation_source(
        &self,
    ) -> Result<
        Arc<dyn awaken_dispatch_contract::DispatchOperationalFeed>,
        AgentCoordinationInstallError,
    > {
        self.host
            .coordinator_dispatch()
            .map(|source| source as Arc<dyn awaken_dispatch_contract::DispatchOperationalFeed>)
            .map_err(|_| AgentCoordinationInstallError::DispatchObservationUnavailable)
    }

    pub fn new(host: Arc<SharedHost>) -> Self {
        Self { host }
    }
}

/// The Session application's executor roles over one Managed adapter's Host:
/// each role is served by the component that owns it.
#[must_use]
pub fn session_execution_roles(
    managed: &ManagedHost,
) -> awaken_session_execution_contract::SessionExecutionRoles {
    let core = Arc::new(managed.clone());
    awaken_session_execution_contract::SessionExecutionRoles {
        core: core.clone(),
        realization: managed.host.run_assembly.clone(),
        coordination: core.clone(),
        coordination_reads: core.clone(),
        budget: core.clone(),
        streaming_runs: core.clone(),
        environment: managed.host.run_assembly.clone(),
        thread_reads: managed.host.thread_reads.clone(),
        terminal_cleanup: managed.host.run_assembly.clone(),
        outcomes: core.clone(),
        live_inbox: managed.host.live_inboxes.clone(),
        capability: core,
    }
}
