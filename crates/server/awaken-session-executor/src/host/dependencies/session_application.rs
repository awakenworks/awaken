// @formal surface exclude=composition_wiring
// @formal owner crates/contract/awaken-session-execution-contract/src/coordination.rs
// @formal owner crates/contract/awaken-session-application-contract/src/run_application.rs
// @formal reason: Process-local weak connections to existing application authorities; no state, queue,
// @formal reason: receipt or admission law is implemented here. Readiness observes only these connections.
//! Startup connections from the execution Host to its existing Session applications.

use std::sync::{Arc, OnceLock, Weak};

use awaken_session_application_contract::SessionRunBackgroundApplication;
use awaken_session_execution_contract::SessionAgentCoordination;

/// Failure to connect a selected Session application before its service starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionApplicationBindingError {
    #[error("this Session application connection is already bound")]
    AlreadyBound,
    #[error("required Session application connection is unavailable: {0}")]
    RequiredApplicationUnavailable(&'static str),
}

/// Shared single-assignment storage, private to Host composition.
struct ApplicationPortBinding<T> {
    slot: Arc<OnceLock<T>>,
}

impl<T> ApplicationPortBinding<T> {
    fn bind(&self, value: T) -> Result<(), SessionApplicationBindingError> {
        self.slot
            .set(value)
            .map_err(|_| SessionApplicationBindingError::AlreadyBound)
    }

    fn get(&self) -> Option<&T> {
        self.slot.get()
    }
}

impl<T> Clone for ApplicationPortBinding<T> {
    fn clone(&self) -> Self {
        Self {
            slot: self.slot.clone(),
        }
    }
}

impl<T> Default for ApplicationPortBinding<T> {
    fn default() -> Self {
        Self {
            slot: Arc::new(OnceLock::new()),
        }
    }
}

/// The Host's connections to existing Session use cases. The composition owns
/// the applications strongly; the Host never prolongs their lifetimes. Clones
/// share the same connections, not independently configurable copies.
#[derive(Clone, Default)]
pub struct SessionApplicationBindings {
    agent_coordination: ApplicationPortBinding<Weak<dyn SessionAgentCoordination>>,
    background_run_submission: ApplicationPortBinding<Weak<dyn SessionRunBackgroundApplication>>,
}

impl SessionApplicationBindings {
    /// Connect the existing coordination application. The passed startup clone
    /// is downgraded here; the composition retains the strong owner.
    pub fn bind_agent_coordination(
        &self,
        application: Arc<dyn SessionAgentCoordination>,
    ) -> Result<(), SessionApplicationBindingError> {
        self.agent_coordination.bind(Arc::downgrade(&application))
    }

    /// Connect the existing Run admission application without creating a
    /// BackgroundTask owner or another Run submission path.
    pub fn bind_background_run_submission(
        &self,
        application: Arc<dyn SessionRunBackgroundApplication>,
    ) -> Result<(), SessionApplicationBindingError> {
        self.background_run_submission
            .bind(Arc::downgrade(&application))
    }

    /// Validate the local coordination service selected by this composition.
    /// Runtime callers still upgrade and authorize at their actual use point.
    pub fn require_agent_coordination(&self) -> Result<(), SessionApplicationBindingError> {
        self.agent_coordination()
            .and_then(Weak::upgrade)
            .map(|_| ())
            .ok_or(
                SessionApplicationBindingError::RequiredApplicationUnavailable(
                    "agent coordination",
                ),
            )
    }

    /// Validate an enabled local background-Run submission service.
    pub fn require_background_run_submission(&self) -> Result<(), SessionApplicationBindingError> {
        self.background_run_submission()
            .and_then(Weak::upgrade)
            .map(|_| ())
            .ok_or(
                SessionApplicationBindingError::RequiredApplicationUnavailable(
                    "background Run submission",
                ),
            )
    }

    pub(crate) fn agent_coordination(&self) -> Option<&Weak<dyn SessionAgentCoordination>> {
        self.agent_coordination.get()
    }

    pub(crate) fn background_run_submission(
        &self,
    ) -> Option<&Weak<dyn SessionRunBackgroundApplication>> {
        self.background_run_submission.get()
    }
}
