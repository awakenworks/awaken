// @formal surface exclude=composition_wiring
// @formal owner crates/contract/awaken-session-execution-contract/src/coordination.rs
// @formal owner crates/contract/awaken-session-application-contract/src/run_application.rs
// @formal reason: The one late binding from the Session executor to the two application roles it calls
// @formal reason: back; it holds no domain state and decides no admission law.
//! The Session executor's one late-bound edge back into the Session application.
//!
//! The application launches child and background Runs through the executor,
//! and the executor calls back into the application for model admission,
//! settlement observation and background submission. That is a true runtime
//! cycle, so construction order alone cannot break it: the executor is built
//! first and receives a [`SessionApplicationBinding`]; the composition root
//! completes the binding once, right after it builds the application.
//!
//! The binding observes the two roles without owning them. The
//! [`SessionApplicationLifetime`] that [`SessionApplicationBinding::bind`]
//! returns owns them, and the composition root keeps it with the execution
//! services, so the executor never keeps the application alive.
//!
//! This is the one listed exemption from construction-time injection. It is
//! removed when the application activates and reads Runs only through the
//! dispatch queue and committed reads, so the executor no longer calls back.
use awaken_session_application_contract::SessionRunBackgroundApplication;
use std::sync::{Arc, OnceLock, Weak};

use crate::SessionAgentCoordination;

/// The application topology a composition root selects once. A local process
/// supplies both roles together; no partially configured mode exists.
/// Standalone executes ordinary Threads without Session applications; a
/// claimed Worker reaches its Session application over the Worker transport.
///
/// Both local roles are mandatory at the public construction boundary:
///
/// ```compile_fail,E0063
/// use std::sync::Arc;
/// use awaken_session_execution_contract::{SessionAgentCoordination, SessionApplications};
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

impl SessionApplications {
    #[must_use]
    pub fn mode(&self) -> SessionApplicationMode {
        match self {
            Self::Local { .. } => SessionApplicationMode::Local,
            Self::Standalone => SessionApplicationMode::Standalone,
            Self::ClaimedWorker => SessionApplicationMode::ClaimedWorker,
        }
    }
}

/// The two local roles a lifetime owns. They are held only to keep the roles
/// alive and are never read: callers reach them through the binding's `Weak`s.
struct LocalApplications {
    _coordination: Arc<dyn SessionAgentCoordination>,
    _background_runs: Arc<dyn SessionRunBackgroundApplication>,
}

/// Ownership of the bound roles, retained by the execution lifecycle outside
/// the executor and its resolver. It keeps a sole consumed `Arc` alive without
/// a cycle between the executor and the application.
#[derive(Clone)]
#[must_use = "retain application ownership until execution services have drained"]
pub struct SessionApplicationLifetime {
    _local: Option<Arc<LocalApplications>>,
}

/// Why the Session application could not be bound into execution or reached
/// through the binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionApplicationBindingError {
    #[error("this Session application connection is already bound")]
    AlreadyBound,
    #[error("select complete Session applications before constructing execution services")]
    Unbound,
    #[error("this process binds no co-located Session application")]
    NoLocalApplication,
    #[error("required Session application connection is unavailable: {0}")]
    RequiredApplicationUnavailable(&'static str),
    #[error("compose Session execution before starting dispatch services")]
    ServicesAlreadyStarted,
    #[error("invalid Session execution topology: {0}")]
    InvalidTopology(&'static str),
}

enum BoundApplications {
    Local {
        coordination: Weak<dyn SessionAgentCoordination>,
        background_runs: Weak<dyn SessionRunBackgroundApplication>,
    },
    Standalone,
    ClaimedWorker,
}

/// The executor's binding to the application roles it calls back. Clones share
/// one cell, which is bound exactly once.
///
/// | State | `bind` | `mode` | a role | `acquire` |
/// |---|---|---|---|---|
/// | unbound | binds; returns the lifetime | `Unbound` | `Unbound` | `Unbound` |
/// | bound, Local, roles alive | `AlreadyBound`, binding unchanged | `Local` | the role | both roles |
/// | bound, Local, a role dropped | `AlreadyBound` | `Local` | `RequiredApplicationUnavailable` | `RequiredApplicationUnavailable` |
/// | bound, Standalone or ClaimedWorker | `AlreadyBound` | that mode | `NoLocalApplication` | an empty lifetime |
///
/// A dropped role never changes the bound mode, so expiry never selects a
/// different authority.
#[derive(Clone, Default)]
pub struct SessionApplicationBinding {
    bound: Arc<OnceLock<BoundApplications>>,
}

impl SessionApplicationBinding {
    /// Complete the binding with the composition root's selection. Only the
    /// first call binds; the returned lifetime owns the local roles.
    pub fn bind(
        &self,
        applications: &SessionApplications,
    ) -> Result<SessionApplicationLifetime, SessionApplicationBindingError> {
        let bound = match applications {
            SessionApplications::Local {
                coordination,
                background_runs,
            } => BoundApplications::Local {
                coordination: Arc::downgrade(coordination),
                background_runs: Arc::downgrade(background_runs),
            },
            SessionApplications::Standalone => BoundApplications::Standalone,
            SessionApplications::ClaimedWorker => BoundApplications::ClaimedWorker,
        };
        self.bound
            .set(bound)
            .map_err(|_| SessionApplicationBindingError::AlreadyBound)?;
        let local = match applications {
            SessionApplications::Local {
                coordination,
                background_runs,
            } => Some(Arc::new(LocalApplications {
                _coordination: coordination.clone(),
                _background_runs: background_runs.clone(),
            })),
            SessionApplications::Standalone | SessionApplications::ClaimedWorker => None,
        };
        Ok(SessionApplicationLifetime { _local: local })
    }

    /// The bound topology.
    pub fn mode(&self) -> Result<SessionApplicationMode, SessionApplicationBindingError> {
        Ok(match self.bound()? {
            BoundApplications::Local { .. } => SessionApplicationMode::Local,
            BoundApplications::Standalone => SessionApplicationMode::Standalone,
            BoundApplications::ClaimedWorker => SessionApplicationMode::ClaimedWorker,
        })
    }

    /// The co-located application's Agent coordination role.
    pub fn agent_coordination(
        &self,
    ) -> Result<Arc<dyn SessionAgentCoordination>, SessionApplicationBindingError> {
        match self.bound()? {
            BoundApplications::Local { coordination, .. } => coordination.upgrade().ok_or(
                SessionApplicationBindingError::RequiredApplicationUnavailable(
                    "agent coordination",
                ),
            ),
            BoundApplications::Standalone | BoundApplications::ClaimedWorker => {
                Err(SessionApplicationBindingError::NoLocalApplication)
            }
        }
    }

    /// The co-located application's background Run submission role.
    pub fn background_runs(
        &self,
    ) -> Result<Arc<dyn SessionRunBackgroundApplication>, SessionApplicationBindingError> {
        match self.bound()? {
            BoundApplications::Local {
                background_runs, ..
            } => background_runs.upgrade().ok_or(
                SessionApplicationBindingError::RequiredApplicationUnavailable(
                    "background Run submission",
                ),
            ),
            BoundApplications::Standalone | BoundApplications::ClaimedWorker => {
                Err(SessionApplicationBindingError::NoLocalApplication)
            }
        }
    }

    /// Acquire both local roles or none, for a service that must keep them
    /// alive while it runs. Failure never changes the bound mode.
    pub fn acquire(&self) -> Result<SessionApplicationLifetime, SessionApplicationBindingError> {
        let local = match self.bound()? {
            BoundApplications::Local {
                coordination,
                background_runs,
            } => {
                let unavailable = SessionApplicationBindingError::RequiredApplicationUnavailable(
                    "local coordination and background Run submission",
                );
                Some(Arc::new(LocalApplications {
                    _coordination: coordination.upgrade().ok_or(unavailable)?,
                    _background_runs: background_runs.upgrade().ok_or(unavailable)?,
                }))
            }
            BoundApplications::Standalone | BoundApplications::ClaimedWorker => None,
        };
        Ok(SessionApplicationLifetime { _local: local })
    }

    fn bound(&self) -> Result<&BoundApplications, SessionApplicationBindingError> {
        self.bound
            .get()
            .ok_or(SessionApplicationBindingError::Unbound)
    }
}

#[cfg(test)]
mod tests;
