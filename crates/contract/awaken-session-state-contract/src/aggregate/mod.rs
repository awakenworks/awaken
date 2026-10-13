// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This aggregate owns the mutable Session candidate, immutable read snapshot and
// @formal reason: revision-bound mutation compilation. Its caller retains authorization and root CAS;
// @formal reason: the product classification does not substitute for those commit/effect checks.
//! Controlled Session working aggregate; the compatible DTO is its read snapshot.

use crate::*;
use awaken_credential_contract::PlaintextHolder;
use awaken_session_configuration_contract::mcp::{
    McpAttachmentDraft, McpAttachmentError, McpGeneration, McpRealizationClaim, McpReplacementPlan,
};
use std::ops::Deref;

/// A validated working candidate for the existing repository CAS. No new
/// durable state or commit authority lives here. Fields are read-only, and
/// commands expose neither a mutable DTO nor an arbitrary mutation callback.
///
/// ```compile_fail,E0594
/// use awaken_session_state_contract::{SessionAggregate, SessionExecutionState};
/// fn cannot_overwrite(root: &mut SessionAggregate) {
///     root.execution = SessionExecutionState::Idle;
/// }
/// ```
///
/// ```compile_fail,E0596
/// use awaken_session_state_contract::SessionAggregate;
/// fn cannot_change_nested(root: &mut SessionAggregate) {
///     root.resources.active = Default::default();
/// }
/// ```
///
/// ```compile_fail,E0596
/// use awaken_session_state_contract::{SessionAggregate, PersistedSession};
/// fn cannot_escape(root: &mut SessionAggregate) -> &mut PersistedSession {
///     root
/// }
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct SessionAggregate {
    state: PersistedSession,
}

impl Deref for SessionAggregate {
    type Target = PersistedSession;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl TryFrom<PersistedSession> for SessionAggregate {
    type Error = SessionMutationValidationError;
    fn try_from(state: PersistedSession) -> Result<Self, Self::Error> {
        if state.session_id.as_str().trim().is_empty() {
            return Err(SessionMutationValidationError::EmptySessionId);
        }
        crate::session_repo::validate_aggregate_state(&state)?;
        Ok(Self { state })
    }
}

impl SessionAggregate {
    /// Read the exact compatible DTO, including historical unknown timestamps.
    pub fn snapshot(&self) -> &PersistedSession {
        &self.state
    }

    /// Consume the working candidate as a read/transport snapshot.
    pub fn into_snapshot(self) -> PersistedSession {
        self.state
    }

    /// Observe an update only after the caller compiled its retry identity.
    /// Repository replay classification precedes command-local validation;
    /// this compiler preserves that ordering and never advances the revision.
    pub fn compile_mutation(
        &mut self,
        owner_scope: &str,
        idempotency: IdempotencyRecord,
        mut lifecycle_facts: Vec<ManagedLifecycleFact>,
        now: u64,
    ) -> SessionMutation {
        // The DTO already retains each interval's exact closing root revision.
        // Derive required facts from those coordinates, with no second ledger.
        if let Some(next) = self.state.revision.0.checked_add(1).map(SessionRevision) {
            for interval in self
                .state
                .closed_runtime_intervals
                .iter()
                .filter(|interval| interval.closed_revision == next)
            {
                let required = ManagedLifecycleFact {
                    id: interval.interval_id.clone(),
                    object_id: self.state.session_id.to_string(),
                    workspace_id: Some(owner_scope.into()),
                    event_type: "session.runtime_interval_closed".into(),
                    timestamp: i64::try_from(interval.ended_at_unix_ms / 1_000).unwrap_or(i64::MAX),
                    runtime_interval: Some(interval.clone()),
                };
                if !lifecycle_facts.contains(&required) {
                    lifecycle_facts.push(required);
                }
            }
        }
        self.state.updated_at_unix_ms =
            session_root_update_observation(self.state.updated_at_unix_ms, now);
        SessionMutation {
            expected_revision: self.state.revision,
            idempotency,
            payload: SessionMutationPayload::Replace(self.state.clone()),
            lifecycle_facts,
        }
    }

    /// Advance only by the next revision returned by the repository's Applied
    /// receipt. Replay must instead reload the repository's durable snapshot.
    pub fn into_applied_snapshot(
        mut self,
        revision: SessionRevision,
    ) -> Result<PersistedSession, SessionMutationValidationError> {
        if self.state.revision.0.checked_add(1) != Some(revision.0) {
            return Err(SessionMutationValidationError::ReplacementRevisionMismatch);
        }
        self.state.revision = revision;
        Ok(self.state)
    }

    /// Stamp an untouched create candidate once, together with its input times.
    /// Recovery uses TryFrom and never invents an unknown creation observation.
    pub fn observe_creation(&mut self, now: u64) -> Result<(), SessionMutationValidationError> {
        if self.state.frozen_baseline().is_none()
            || self.state.revision.0 != 0
            || self.state.created_at_unix_ms.is_some()
            || self.state.updated_at_unix_ms.is_some()
        {
            return Err(SessionMutationValidationError::CreationObservationChanged);
        }
        self.state
            .resources
            .active
            .observe_input_times(&Default::default(), now);
        if let Some(pending) = &mut self.state.resources.pending {
            pending.observe_input_times(&Default::default(), now);
        }
        self.state.created_at_unix_ms = Some(now);
        self.state.updated_at_unix_ms = Some(now);
        Ok(())
    }
    /// Controlled activity commands on this working root.
    pub fn activity(&mut self) -> SessionActivityOperations<'_> {
        SessionActivityOperations { aggregate: self }
    }
    /// Controlled lifecycle commands on this working root.
    pub fn lifecycle(&mut self) -> SessionLifecycleOperations<'_> {
        SessionLifecycleOperations { aggregate: self }
    }
    /// Controlled cleanup commands on this working root.
    pub fn cleanup(&mut self) -> SessionCleanupOperations<'_> {
        SessionCleanupOperations { aggregate: self }
    }
    /// Controlled environment commands on this working root.
    pub fn environment(&mut self) -> SessionEnvironmentOperations<'_> {
        SessionEnvironmentOperations { aggregate: self }
    }
    /// Controlled resources commands on this working root.
    pub fn resources(&mut self) -> SessionResourcesOperations<'_> {
        SessionResourcesOperations { aggregate: self }
    }
    /// Controlled mcp commands on this working root.
    pub fn mcp(&mut self) -> SessionMcpOperations<'_> {
        SessionMcpOperations { aggregate: self }
    }
    /// Controlled configuration commands on this working root.
    pub fn configuration(
        &mut self,
    ) -> Result<SessionConfigurationOperations<'_>, SessionAggregateCommandError> {
        if self.state.is_terminal() || self.state.execution != SessionExecutionState::Idle {
            return Err(SessionAggregateCommandError::ConfigurationNotIdle);
        }
        Ok(SessionConfigurationOperations { aggregate: self })
    }
    /// Controlled realization commands on this working root.
    pub fn assignment(&mut self) -> SessionAssignmentOperations<'_> {
        SessionAssignmentOperations { aggregate: self }
    }
    /// Controlled budget commands on this working root.
    pub fn budget(&mut self) -> SessionBudgetOperations<'_> {
        SessionBudgetOperations { aggregate: self }
    }
    /// Controlled events commands on this working root.
    pub fn events(&mut self) -> SessionEventsOperations<'_> {
        SessionEventsOperations { aggregate: self }
    }
}

/// Borrowed activity command capability; no mutable snapshot escape.
pub struct SessionActivityOperations<'a> {
    aggregate: &'a mut SessionAggregate,
}
mod activity;
pub use activity::{SessionActivityAdmissionPlan, session_activity_open_plan};
/// Borrowed lifecycle command capability; no mutable snapshot escape.
pub struct SessionLifecycleOperations<'a> {
    aggregate: &'a mut SessionAggregate,
}
mod lifecycle;
/// Borrowed cleanup command capability; no mutable snapshot escape.
pub struct SessionCleanupOperations<'a> {
    aggregate: &'a mut SessionAggregate,
}
mod cleanup;
mod cleanup_extra;
/// Borrowed environment command capability; no mutable snapshot escape.
pub struct SessionEnvironmentOperations<'a> {
    aggregate: &'a mut SessionAggregate,
}
mod environment;
mod environment_extra;
impl Deref for SessionEnvironmentOperations<'_> {
    type Target = SessionEnvironmentState;
    fn deref(&self) -> &Self::Target {
        &self.aggregate.state.environment
    }
}
/// Borrowed resources command capability; no mutable snapshot escape.
pub struct SessionResourcesOperations<'a> {
    aggregate: &'a mut SessionAggregate,
}
mod resource_credentials;
mod resources;
impl Deref for SessionResourcesOperations<'_> {
    type Target = SessionResourceState;
    fn deref(&self) -> &Self::Target {
        &self.aggregate.state.resources
    }
}
/// Borrowed mcp command capability; no mutable snapshot escape.
pub struct SessionMcpOperations<'a> {
    aggregate: &'a mut SessionAggregate,
}
mod mcp;
impl Deref for SessionMcpOperations<'_> {
    type Target = SessionMcpAttachmentSet;
    fn deref(&self) -> &Self::Target {
        &self.aggregate.state.mcp
    }
}
/// Borrowed configuration command capability; no mutable snapshot escape.
pub struct SessionConfigurationOperations<'a> {
    aggregate: &'a mut SessionAggregate,
}
mod configuration;
/// Borrowed realization command capability; no mutable snapshot escape.
pub struct SessionAssignmentOperations<'a> {
    aggregate: &'a mut SessionAggregate,
}
mod assignment;
/// Borrowed budget command capability; no mutable snapshot escape.
pub struct SessionBudgetOperations<'a> {
    aggregate: &'a mut SessionAggregate,
}
mod budget;
impl Deref for SessionBudgetOperations<'_> {
    type Target = SessionBudgetState;
    fn deref(&self) -> &Self::Target {
        &self.aggregate.state.budget
    }
}
/// Borrowed events command capability; no mutable snapshot escape.
pub struct SessionEventsOperations<'a> {
    aggregate: &'a mut SessionAggregate,
}
mod events;

/// Typed rejection of a controlled root command. Rejection changes no durable state.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionAggregateCommandError {
    #[error("Session configuration requires a nonterminal Idle root")]
    ConfigurationNotIdle,
    #[error("Session frozen authoring policy does not permit changing tools")]
    FrozenTools,
    #[error("MCP projection lease cannot advance: {0}")]
    ProjectionLease(String),
    #[error("Session realization lease epoch is not its exact successor")]
    InvalidLeaseGeneration,
    #[error("Session realization attempt counter is exhausted")]
    AttemptCounterExhausted,
    #[error("Session realization has no incumbent lease")]
    MissingIncumbentLease,
    #[error("Session realization renewal changed or regressed its generation")]
    InvalidLeaseRenewal,
    #[error("Session realization has no attempt to refund")]
    MissingCapacityAttempt,
    #[error("Session realization claim no longer owns compensation")]
    StaleCompensation,
    #[error("Session realization compensation names another predecessor")]
    InvalidPredecessor,
    #[error("a budget cannot be added to a Session created without one")]
    BudgetAbsent,
    #[error("a removed Session budget cannot be re-added")]
    BudgetRemoved,
    #[error("updated max_list_cost must be greater than consumed list cost")]
    BudgetNotAboveConsumed,
    #[error("Session is not ready for run-free preparation")]
    PreparationNotReady,
}

/// Pure observation kernel shared by mutation compilation and formal checks.
pub const fn session_root_update_observation(previous: Option<u64>, now: u64) -> Option<u64> {
    Some(match previous {
        Some(previous) if previous > now => previous,
        _ => now,
    })
}
