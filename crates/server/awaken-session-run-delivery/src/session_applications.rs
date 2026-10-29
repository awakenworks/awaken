// @formal surface exclude=composition_wiring
// @formal owner crates/contract/awaken-session-execution-contract/src/coordination.rs
// @formal owner crates/contract/awaken-session-application-contract/src/run_application.rs
// @formal reason: Retains existing application ports for one process lifecycle; no domain state or admission law.
//! Complete application selection and its process-owned lifetime.
use awaken_session_application_contract::SessionRunBackgroundApplication;
use awaken_session_execution_contract::SessionAgentCoordination;
use std::sync::{Arc, Weak};

/// The application capabilities selected before execution services are built.
/// A local process supplies both roles together; no partially configured mode
/// exists. Standalone executes ordinary Threads without Session applications.
///
/// Both local roles are mandatory at the public construction boundary:
///
/// ```compile_fail,E0063
/// use std::sync::Arc;
/// use awaken_session_run_delivery::SessionApplications;
/// use awaken_session_execution_contract::SessionAgentCoordination;
/// fn incomplete(coordination: Arc<dyn SessionAgentCoordination>) {
///     let _ = SessionApplications::Local { coordination };
/// }
/// ```
pub enum SessionApplications {
    Local {
        coordination: Arc<dyn SessionAgentCoordination>,
        background_runs: Arc<dyn SessionRunBackgroundApplication>,
    },
    Standalone,
    ClaimedWorker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionApplicationMode {
    Local,
    Standalone,
    ClaimedWorker,
}

struct LocalApplications {
    coordination: Arc<dyn SessionAgentCoordination>,
    background_runs: Arc<dyn SessionRunBackgroundApplication>,
}

/// Ownership retained by the lifecycle, outside the Host and its resolver.
/// This keeps a sole consumed Arc alive without introducing a Host/app cycle.
#[derive(Clone)]
#[must_use = "retain application ownership until execution services have drained"]
pub struct SessionApplicationLifetime {
    _local: Option<Arc<LocalApplications>>,
}

/// Weak observations shared by Host capabilities after one complete selection.
/// Consumers may inspect them, but only the Host chooses the immutable mode.
pub struct SessionApplicationConnections {
    mode: SessionApplicationMode,
    coordination: Option<Weak<dyn SessionAgentCoordination>>,
    background_runs: Option<Weak<dyn SessionRunBackgroundApplication>>,
}

impl SessionApplications {
    #[must_use]
    pub fn mode(&self) -> SessionApplicationMode {
        match self {
            Self::Local { .. } => SessionApplicationMode::Local,
            Self::Standalone => SessionApplicationMode::Standalone,
            Self::ClaimedWorker => SessionApplicationMode::ClaimedWorker,
        }
    }

    /// Consume the complete selection, returning observations and their owner.
    pub fn connect(self) -> (SessionApplicationConnections, SessionApplicationLifetime) {
        let mode = self.mode();
        let local = match self {
            Self::Local {
                coordination,
                background_runs,
            } => Some(Arc::new(LocalApplications {
                coordination,
                background_runs,
            })),
            Self::Standalone | Self::ClaimedWorker => None,
        };
        let connections = SessionApplicationConnections {
            mode,
            coordination: local.as_ref().map(|p| Arc::downgrade(&p.coordination)),
            background_runs: local.as_ref().map(|p| Arc::downgrade(&p.background_runs)),
        };
        (connections, SessionApplicationLifetime { _local: local })
    }
}

impl SessionApplicationConnections {
    #[must_use]
    pub fn mode(&self) -> SessionApplicationMode {
        self.mode
    }
    #[must_use]
    pub fn agent_coordination(&self) -> Option<&Weak<dyn SessionAgentCoordination>> {
        self.coordination.as_ref()
    }
    #[must_use]
    pub fn background_run_submission(&self) -> Option<&Weak<dyn SessionRunBackgroundApplication>> {
        self.background_runs.as_ref()
    }

    /// Acquire both required capabilities or none. Failure never changes mode
    /// and never selects a different authority after a local owner expires.
    #[must_use]
    pub fn acquire(&self) -> Option<SessionApplicationLifetime> {
        let local = match self.mode {
            SessionApplicationMode::Local => Some(Arc::new(LocalApplications {
                coordination: self.coordination.as_ref()?.upgrade()?,
                background_runs: self.background_runs.as_ref()?.upgrade()?,
            })),
            SessionApplicationMode::Standalone | SessionApplicationMode::ClaimedWorker => None,
        };
        Some(SessionApplicationLifetime { _local: local })
    }
}

#[cfg(test)]
mod tests;
