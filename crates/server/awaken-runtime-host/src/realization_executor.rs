//! The realization executor (ADR-0089): executes the Environment realization
//! effects the Session aggregate projects, through ports chosen once when the
//! Host is built. It keeps no durable state.

/// Executes Session Environment realization through its components. Holding
/// each Session's Environment is its primary role, reached through `Deref`.
pub struct RealizationExecutor {
    /// The Environment each installed Session holds, and its providers.
    pub(crate) environments: crate::environment_holding::EnvironmentHolding,
    /// Terminal cleanup authorization.
    pub(crate) cleanup: crate::terminal_cleanup::TerminalCleanup,
}

impl std::ops::Deref for RealizationExecutor {
    type Target = crate::environment_holding::EnvironmentHolding;

    fn deref(&self) -> &Self::Target {
        &self.environments
    }
}
