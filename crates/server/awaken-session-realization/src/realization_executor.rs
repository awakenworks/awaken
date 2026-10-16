// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Select the binding sink at construction and execute through the existing canonical Session slots and provider authority.
//! The realization executor (ADR-0089): executes the Environment realization
//! effects the Session aggregate projects, through ports chosen once when the
//! Host is built. It keeps no durable state.

/// Executes Session Environment realization through its components. Holding
/// each Session's Environment is its primary role, reached through `Deref`.
pub struct RealizationExecutor {
    /// The same process clock that authorizes installed realization leases.
    pub clock: std::sync::Arc<dyn awaken_time::WallClock>,
    /// The Environment each installed Session holds, and its providers.
    pub environments: crate::environment_holding::EnvironmentHolding,
    /// Terminal cleanup authorization.
    pub cleanup: crate::terminal_cleanup::TerminalCleanup,
}

impl std::ops::Deref for RealizationExecutor {
    type Target = crate::environment_holding::EnvironmentHolding;

    fn deref(&self) -> &Self::Target {
        &self.environments
    }
}
