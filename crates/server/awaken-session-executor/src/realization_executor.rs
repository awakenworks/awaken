// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Select the binding sink at construction and execute through the existing canonical Session slots and provider authority.
//! The realization executor (ADR-0089): executes the Environment realization
//! effects the Session aggregate projects, through ports chosen once when the
//! Host is built. It keeps no durable state.

/// Executes Session Environment realization through its components. Holding
/// each Session's Environment is its primary role, reached through `Deref`.
pub struct RealizationExecutor {
    /// The selected aggregate publication authority; the lock permits fixture installation only.
    pub(crate) environment_binding_sink: std::sync::RwLock<
        Option<std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>,
    >,
    /// The same dispatch instance used by this process, narrowed to execution binding.
    pub(crate) dispatch_execution:
        Option<std::sync::Arc<dyn awaken_dispatch_contract::DispatchExecution>>,
    /// Frozen publication selection over the installed Session slots.
    pub(crate) publication: crate::session_publication::SessionPublicationSelection,
    /// The same process clock that authorizes installed realization leases.
    pub(crate) clock: std::sync::Arc<dyn awaken_time::WallClock>,
    /// The Environment each installed Session holds, and its providers.
    pub(crate) environments: crate::environment_holding::EnvironmentHolding,
    /// The single output publication operation shared by Run and cleanup effects.
    pub(crate) artifacts: crate::provisioning::ArtifactHarvester,
    /// Terminal cleanup authorization.
    pub(crate) cleanup: crate::terminal_cleanup::TerminalCleanup,
}

impl std::ops::Deref for RealizationExecutor {
    type Target = crate::environment_holding::EnvironmentHolding;

    fn deref(&self) -> &Self::Target {
        &self.environments
    }
}
