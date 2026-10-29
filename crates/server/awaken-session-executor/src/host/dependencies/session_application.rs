// @formal surface exclude=composition_wiring
// @formal owner crates/contract/awaken-session-execution-contract/src/coordination.rs
// @formal owner crates/contract/awaken-session-application-contract/src/run_application.rs
// @formal reason: One immutable complete process selection; no domain admission, queue or durable state.
//! Host-owned atomic selection of the existing application capabilities.
use awaken_session_application_contract::SessionRunBackgroundApplication;
use awaken_session_execution_contract::SessionAgentCoordination;
use awaken_session_run_delivery::{
    SessionApplicationConnections, SessionApplicationLifetime, SessionApplicationMode,
};
use std::sync::{Arc, OnceLock, Weak};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionApplicationBindingError {
    #[error("this Session application connection is already bound")]
    AlreadyBound,
    #[error("required Session application connection is unavailable: {0}")]
    RequiredApplicationUnavailable(&'static str),
    #[error("select complete Session applications before constructing execution services")]
    NotSelected,
    #[error("compose Session execution before starting dispatch services")]
    ServicesAlreadyStarted,
    #[error("invalid Session execution topology: {0}")]
    InvalidTopology(&'static str),
}

/// Clones share one immutable complete selection. Only checked execution
/// service construction may publish it; callers cannot bind individual roles.
#[derive(Clone, Default)]
pub struct SessionApplicationBindings {
    selected: Arc<OnceLock<SessionApplicationConnections>>,
}
impl SessionApplicationBindings {
    pub(crate) fn select_connections(
        &self,
        connections: SessionApplicationConnections,
    ) -> Result<(), SessionApplicationBindingError> {
        self.selected
            .set(connections)
            .map_err(|_| SessionApplicationBindingError::AlreadyBound)
    }
    #[must_use]
    pub fn mode(&self) -> Option<SessionApplicationMode> {
        self.selected.get().map(SessionApplicationConnections::mode)
    }
    pub(crate) fn acquire(
        &self,
    ) -> Result<SessionApplicationLifetime, SessionApplicationBindingError> {
        self.selected
            .get()
            .ok_or(SessionApplicationBindingError::NotSelected)?
            .acquire()
            .ok_or(
                SessionApplicationBindingError::RequiredApplicationUnavailable(
                    "local coordination and background Run submission",
                ),
            )
    }
    pub(crate) fn agent_coordination(&self) -> Option<&Weak<dyn SessionAgentCoordination>> {
        self.selected.get()?.agent_coordination()
    }
    pub(crate) fn background_run_submission(
        &self,
    ) -> Option<&Weak<dyn SessionRunBackgroundApplication>> {
        self.selected.get()?.background_run_submission()
    }
    #[cfg(test)]
    pub(crate) fn require_agent_coordination(&self) -> Result<(), SessionApplicationBindingError> {
        self.agent_coordination()
            .and_then(Weak::upgrade)
            .map(|_| ())
            .ok_or(
                SessionApplicationBindingError::RequiredApplicationUnavailable(
                    "agent coordination",
                ),
            )
    }
    #[cfg(test)]
    pub(crate) fn require_background_run_submission(
        &self,
    ) -> Result<(), SessionApplicationBindingError> {
        self.background_run_submission()
            .and_then(Weak::upgrade)
            .map(|_| ())
            .ok_or(
                SessionApplicationBindingError::RequiredApplicationUnavailable(
                    "background Run submission",
                ),
            )
    }
}
