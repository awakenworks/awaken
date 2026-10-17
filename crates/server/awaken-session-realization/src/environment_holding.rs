// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The Environment component owns the original per-Session lifecycle mutex and physical owner projection; no second registry or durable authority is introduced.
//! Environment holding of the realization executor: the Environment each
//! installed Session holds, through the sandbox providers it realizes with.
//! It selects the provider, creates, adopts, restores and retires the exact
//! Environment owner under the Session's lifecycle lock, and keeps the warm
//! capacity those providers prepare.

pub mod bound_environment;
pub mod lifecycle;
mod peer;
pub mod realization;

/// Holds each installed Session's Environment through the sandbox providers it
/// owns. Resource staging and the execution authority are peers: an
/// operation that consults them receives them from the executor.
pub struct EnvironmentHolding {
    /// The selected aggregate publication authority; the lock permits fixture installation only.
    environment_binding_sink: std::sync::RwLock<
        Option<std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>,
    >,
    /// The same dispatch instance used by this process, narrowed to execution binding.
    dispatch_execution: Option<std::sync::Arc<dyn awaken_dispatch_contract::DispatchExecution>>,
    /// Frozen publication selection over the installed Sessions.
    publication: crate::session_publication::SessionPublicationSelection,
    /// The Session-owned, BackendOwned and housekeeping sandbox providers.
    providers: crate::environment_providers::EnvironmentProviders,
    /// The installed Sessions, as far as realization may reach them.
    sessions: crate::session_realizations::SessionRealizations,
}

impl EnvironmentHolding {
    /// Hold Environments over the installed Sessions with the sandbox providers
    /// they realize through.
    pub fn new(
        mut providers: crate::environment_providers::EnvironmentProviders,
        runs_local_dispatch_pool: bool,
        sessions: crate::session_realizations::SessionRealizations,
        environment_binding_sink: Option<
            std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
        >,
        dispatch_execution: Option<
            std::sync::Arc<dyn awaken_dispatch_contract::DispatchExecution>,
        >,
        publication: crate::session_publication::SessionPublicationSelection,
    ) -> Self {
        providers.runs_local_dispatch_pool = runs_local_dispatch_pool;
        Self {
            environment_binding_sink: std::sync::RwLock::new(environment_binding_sink),
            dispatch_execution,
            publication,
            providers,
            sessions,
        }
    }

    /// The binding sink of a test fixture.
    #[cfg(any(test, feature = "test-support"))]
    pub fn environment_binding_sink(
        &self,
    ) -> Option<std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>
    {
        self.environment_binding_sink
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the dispatch execution role of a test fixture.
    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_dispatch_execution(
        &mut self,
        dispatch_execution: Option<
            std::sync::Arc<dyn awaken_dispatch_contract::DispatchExecution>,
        >,
    ) {
        self.dispatch_execution = dispatch_execution;
    }

    /// Replace the binding sink of a test fixture.
    #[cfg(any(test, feature = "test-support"))]
    pub fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        *self
            .environment_binding_sink
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }
}
