//! Canonical prepared role surfaces and borrowed Coordinator ports.

use axum::Router;
use std::sync::Arc;

/// Role-owned HTTP surfaces plus the cleartext local setup handoff printed by
/// the CLI once. `private_router` is never merged into `public_router`.
#[derive(Clone)]
pub struct CoordinatorAuthorityHandles {
    pub runtime_authority: Arc<dyn awaken_session_executor::RuntimeAuthority>,
    pub worker_directory: Arc<dyn awaken_worker_contract::WorkerDirectory>,
    pub sessions: Arc<dyn awaken_session_repository_contract::SessionRepository>,
    /// The one process-owned Coordinator pool. Hosted adapters and the
    /// process-readiness probe clone this handle rather than reconnecting.
    pub postgres_pool: Option<sqlx::PgPool>,
    pub run_recovery:
        Option<Arc<dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource>>,
    pub run_lifecycle:
        Option<Arc<dyn awaken_agent_contract::thread::read::lifecycle::RunLifecycleFeed>>,
}

pub struct PreparedProcess {
    pub public_router: Router,
    pub private_router: Router,
    pub local_setup: Option<awaken_control::LocalSetupHandoff>,
    pub registration_supervisor: Option<Arc<awaken_control::StaticRegistrationSupervisor>>,
    pub service_lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    /// Read-only projection published by the exact Session application's sole
    /// lifecycle supervisor.
    pub event_batch_cutover_validation:
        Option<Arc<awaken_session_application::SessionEventBatchCutoverValidationSource>>,
    /// Canonical Coordinator persistence handles for a hosted composition.
    /// Consumers must reuse these handles and must not reopen the same stores.
    pub coordinator_authorities: Option<CoordinatorAuthorityHandles>,
    /// Exact selected Worker authentication port shared by hosted private routes.
    pub worker_authenticator:
        Option<Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>>,
    pub admin_tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
    /// The process's one wall clock, shared with an embedded Worker.
    pub clock: Arc<dyn awaken_time::WallClock>,
}

pub(super) struct ProcessRouters {
    pub(super) public_router: Router,
    pub(super) private_router: Router,
    pub(super) registration_supervisor: Option<Arc<awaken_control::StaticRegistrationSupervisor>>,
    pub(super) service_lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    pub(super) event_batch_cutover_validation:
        Option<Arc<awaken_session_application::SessionEventBatchCutoverValidationSource>>,
    pub(super) worker_authenticator:
        Option<Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>>,
    pub(super) admin_tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
}

impl ProcessRouters {
    pub(super) fn new(
        public_router: Router,
        private_router: Router,
        registration_supervisor: Option<Arc<awaken_control::StaticRegistrationSupervisor>>,
        service_lifecycle: awaken_service_lifecycle::ServiceLifecycle,
        event_batch_cutover_validation: Option<
            Arc<awaken_session_application::SessionEventBatchCutoverValidationSource>,
        >,
        admin_tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
    ) -> Self {
        // Router-only embedding helpers do not retain PreparedProcess. Keep the
        // same supervisor alive inside the router as well as exposing it to the
        // binary's readiness controller.
        let public_router = match &registration_supervisor {
            Some(supervisor) => public_router.layer(axum::Extension(supervisor.clone())),
            None => public_router,
        };
        let public_router = public_router.layer(axum::Extension(service_lifecycle.clone()));
        Self {
            public_router,
            private_router,
            registration_supervisor,
            service_lifecycle,
            event_batch_cutover_validation,
            worker_authenticator: None,
            admin_tools,
        }
    }

    pub(super) fn with_worker_authenticator(
        mut self,
        authenticator: Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>,
    ) -> Self {
        self.worker_authenticator = Some(authenticator);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_worker_authentication_is_the_exact_selected_port() {
        // C1 Coordinator supplies the selected authentication Arc -> same Arc
        // is exposed to hosted private routes; C2 Control-only preparation ->
        // absent. This is a borrowed composition port, never a new verifier,
        // Registry reader or credential cache.
        let source: Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator> =
            Arc::new(awaken_worker_transport_security::HeaderWorkerAuthenticator);
        let empty = || {
            ProcessRouters::new(
                Router::new(),
                Router::new(),
                None,
                awaken_service_lifecycle::ServiceLifecycle::new(),
                None,
                Vec::new(),
            )
        };
        assert!(empty().worker_authenticator.is_none(), "C2");
        let prepared = empty().with_worker_authenticator(source.clone());
        assert!(
            Arc::ptr_eq(prepared.worker_authenticator.as_ref().unwrap(), &source),
            "C1 one selected instance"
        );
    }
}
