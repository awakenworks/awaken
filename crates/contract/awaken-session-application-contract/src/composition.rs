// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Process configuration freezes placement, realization owner and retry budget before
// @formal reason: Session admission; reconciliation results project the durable root. This product
// @formal reason: requirement link does not claim a formal proof of process assembly or delivery.
//! Immutable process configuration and application reconciliation projections.

use awaken_session_types::SessionRuntimePlacement;

/// Process-level execution topology interpreted by the Session application.
/// The selected value is frozen into every newly admitted Session; protocol
/// adapters never inspect or override it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionExecutionPlacement {
    #[default]
    LocalWorker,
    RegisteredWorker,
}

/// Freeze process topology into the only two executable Session placements.
#[must_use]
pub const fn runtime_placement_for_topology(
    topology: SessionExecutionPlacement,
) -> SessionRuntimePlacement {
    match topology {
        SessionExecutionPlacement::LocalWorker => SessionRuntimePlacement::Local,
        SessionExecutionPlacement::RegisteredWorker => SessionRuntimePlacement::Worker,
    }
}

/// One failed durable Session-to-WorkQueue projection. The durable Session
/// remains authoritative and a later reconciliation may retry this failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkDispatchFailure {
    pub session_id: String,
    pub environment_id: String,
    pub message: String,
}

/// Result of one complete reconciliation scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkDispatchReconciliation {
    pub settled: usize,
    pub failures: Vec<WorkDispatchFailure>,
    pub quarantined: Vec<awaken_session_types::SessionRecoveryQuarantine>,
    pub pending: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionApplicationConfiguration {
    pub execution_placement: SessionExecutionPlacement,
    /// Stable logical owner for the co-located Runtime. The process-unique
    /// incarnation remains separate so a restart can fence its predecessor
    /// without impersonating a different owner.
    pub local_realization_owner: String,
    /// Maximum fenced assignments allowed for initial Environment
    /// realization before the aggregate enters `activation_failed`.
    pub realization_retry_budget: u32,
}

impl Default for SessionApplicationConfiguration {
    fn default() -> Self {
        Self {
            execution_placement: SessionExecutionPlacement::default(),
            local_realization_owner: "local-runtime".into(),
            realization_retry_budget: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionApplicationBuildError {
    #[error("local Session realization owner must not be empty")]
    EmptyRealizationOwner,
    #[error("Session realization retry budget must be positive")]
    EmptyRealizationRetryBudget,
}

impl SessionApplicationConfiguration {
    /// Validate and normalize process inputs before a service observes a clock.
    pub fn validate(mut self) -> Result<Self, SessionApplicationBuildError> {
        self.local_realization_owner = self.local_realization_owner.trim().into();
        if self.local_realization_owner.is_empty() {
            return Err(SessionApplicationBuildError::EmptyRealizationOwner);
        }
        if self.realization_retry_budget == 0 {
            return Err(SessionApplicationBuildError::EmptyRealizationRetryBudget);
        }
        Ok(self)
    }
}
