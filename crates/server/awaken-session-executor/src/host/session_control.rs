use super::*;

impl SharedHost {
    /// Install the sole outbound claim-fenced client for frozen Session
    /// realization on a Worker.
    #[must_use]
    pub fn with_session_realization_control(
        mut self,
        control: Arc<dyn awaken_dispatch_contract::ClaimedSessionRealizationControl>,
    ) -> Self {
        self.session_control = Some(control);
        self
    }

    #[must_use]
    pub fn with_session_coordination_control(
        mut self,
        control: Arc<dyn awaken_dispatch_contract::ClaimedSessionCoordinationControl>,
    ) -> Self {
        self.session_coordination_control = Some(control);
        self
    }

    /// Install both independent claim-fenced Session roles from the canonical
    /// Worker Control client.
    #[must_use]
    pub fn with_session_control<T>(mut self, control: Arc<T>) -> Self
    where
        T: awaken_dispatch_contract::ClaimedSessionRealizationControl
            + awaken_dispatch_contract::ClaimedSessionCoordinationControl
            + 'static,
    {
        self.session_control = Some(control.clone());
        self.session_coordination_control = Some(control);
        self
    }

    /// Install realization resume and terminal cleanup from one recovery
    /// client without claiming that it owns Agent coordination.
    #[must_use]
    pub fn with_session_recovery_control<T>(mut self, control: Arc<T>) -> Self
    where
        T: awaken_dispatch_contract::ClaimedSessionRealizationControl
            + awaken_session_contract::SessionTerminalCleanupControl
            + 'static,
    {
        self.session_control = Some(control.clone());
        self.terminal_cleanup_control = Some(control);
        self
    }

    /// Install the independent terminal-cleanup role carried by the same
    /// authenticated Worker Control client in production.
    #[must_use]
    pub fn with_terminal_cleanup_control(
        mut self,
        control: Arc<dyn awaken_session_contract::SessionTerminalCleanupControl>,
    ) -> Self {
        self.terminal_cleanup_control = Some(control);
        self
    }
}
