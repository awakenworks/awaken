// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Assignment grant, claim, renewal, expiry and capacity recovery mutate this Session
// @formal reason: candidate under the original epoch/generation laws before root CAS. This classification
// @formal reason: does not extend those laws to external Worker execution or delivery.
//! Realization-lease assignment on the Session aggregate.
//!
//! The durable `SessionRealizationLease` is Session state, and ADR-0089 D1
//! puts every lease decision -- grant, reuse, reassignment, renewal and
//! expiry -- in the aggregate, committed through the Session root CAS. This
//! module decides them over the loaded candidate only; the realization
//! executor carries out the effects the committed root then projects.
use super::realization_lease_generation_authorizes;
use super::{
    McpAttachmentError, McpRealizationClaim, ResourceActivationError, SessionAggregateCommandError,
    SessionAssignmentOperations, SessionEnvironmentEvent, SessionEnvironmentFacts,
    SessionEnvironmentRejection, SessionExecutionState, SessionRealizationLease,
    SessionResourceManifest, stable_fingerprint, transition_environment,
};
use awaken_lease_contract::fence::Epoch;
use awaken_session_configuration_contract::mcp::McpAttachmentState;
use awaken_session_configuration_contract::resource::realization_lease_is_live_at;

/// Assignment result for one atomically loaded Session realization fence.
/// The aggregate applies the returned epoch in the same root-CAS commit that
/// installs the owner; callers cannot mint or reinterpret an epoch themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRealizationAssignment {
    Reuse,
    Assign { epoch: u64 },
    StaleOwnership,
    EpochExhausted,
}

/// Closed owner/incarnation/epoch arbitration used by the real Session root
/// mutation. `current_epoch` is `None` only when no fence has ever existed.
#[must_use]
pub const fn session_realization_assignment(
    current_epoch: Option<u64>,
    current_is_live: bool,
    same_owner: bool,
    same_runtime_incarnation: bool,
    reassign_existing_lease: bool,
) -> SessionRealizationAssignment {
    if current_is_live && !same_owner && !reassign_existing_lease {
        return SessionRealizationAssignment::StaleOwnership;
    }
    let needs_assignment = !current_is_live || !same_runtime_incarnation || reassign_existing_lease;
    if !needs_assignment {
        return SessionRealizationAssignment::Reuse;
    }
    match current_epoch {
        None => SessionRealizationAssignment::Assign { epoch: 1 },
        Some(epoch) => match epoch.checked_add(1) {
            Some(epoch) => SessionRealizationAssignment::Assign { epoch },
            None => SessionRealizationAssignment::EpochExhausted,
        },
    }
}

/// One Runtime asking to drive this Session's realization. The caller has
/// already validated the command shape and resolved the root's Workspace.
#[derive(Clone, Copy, Debug)]
pub struct SessionRealizationBeginRequest<'a> {
    owner_scope: &'a str,
    owner: &'a str,
    runtime_incarnation: &'a str,
    lease_expires_at_unix_ms: u64,
    reassign_existing_lease: bool,
    expected_resources: Option<&'a SessionResourceManifest>,
}

impl<'a> SessionRealizationBeginRequest<'a> {
    /// `owner_scope` is the Workspace that owns the loaded root;
    /// `reassign_existing_lease` explicitly replaces a live lease held by
    /// another logical Runtime; `expected_resources` is the exact Resource
    /// generation a guarded dispatch claimed (`None` selects the current
    /// desired state).
    #[must_use]
    pub const fn new(
        owner_scope: &'a str,
        owner: &'a str,
        runtime_incarnation: &'a str,
        lease_expires_at_unix_ms: u64,
        reassign_existing_lease: bool,
        expected_resources: Option<&'a SessionResourceManifest>,
    ) -> Self {
        Self {
            owner_scope,
            owner,
            runtime_incarnation,
            lease_expires_at_unix_ms,
            reassign_existing_lease,
            expected_resources,
        }
    }
}

/// What the caller does after `begin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRealizationBegin {
    /// The live lease already belongs to this incarnation and no MCP
    /// generation awaits a claim: the root is unchanged, drive it as loaded.
    Unchanged { activate_pending_resources: bool },
    /// The root now carries the claim; commit it, then drive the committed
    /// root. `assigned` means a new owner generation was installed, so the
    /// whole frozen projection must be prepared again.
    Claimed {
        assigned: bool,
        activate_pending_resources: bool,
    },
}

/// Why `begin` refused. A refusal leaves the candidate unchanged except for
/// the three command failures, which the caller discards with the candidate.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionRealizationBeginRejection {
    #[error(transparent)]
    Environment(SessionEnvironmentRejection),
    #[error("claimed Resource Workspace does not match the Session")]
    ResourceWorkspaceMismatch,
    #[error("claimed Resource generation is neither active nor pending")]
    ResourceGenerationNotRetained,
    #[error("Session realization ownership is stale")]
    StaleOwnership,
    #[error("Session realization lease epoch is exhausted")]
    EpochExhausted,
    #[error(transparent)]
    Resources(ResourceActivationError),
    #[error(transparent)]
    Mcp(McpAttachmentError),
    #[error(transparent)]
    Assignment(SessionAggregateCommandError),
}

impl SessionAssignmentOperations<'_> {
    /// Grant or reuse the realization lease and claim what it must realize.
    ///
    /// | step | facts                                         | decision                                   |
    /// |------|-----------------------------------------------|--------------------------------------------|
    /// | B1   | `BeginRealization` refused                    | `Environment(rejection)`                   |
    /// | B2   | expected Workspace differs                    | `ResourceWorkspaceMismatch`                |
    /// | B3   | expected generation is not retained           | `ResourceGenerationNotRetained`            |
    /// | B4   | expected generation absent / not active       | activate pending Resources                 |
    /// | B5   | expected generation is the active one         | keep the active Resources                  |
    /// | B6   | assignment `StaleOwnership` / `EpochExhausted`| the same refusal                           |
    /// | B7   | `Reuse` and no Requested MCP generation       | `Unchanged`                                |
    /// | B8   | `Reuse` with Requested MCP generations        | claim them (realization law), same lease   |
    /// | B9   | `Assign { epoch }`                            | new lease; claim Requested, Realizing and Active (recovery law) |
    /// | B10  | B8/B9 with pending Resources selected (B4)    | start the pending Resource attempt         |
    /// | B11  | B9 while Preparing / Activating               | the claim counts an initial attempt        |
    ///
    /// Each claim's realization id and stage key derive only from the Session,
    /// the generation and the lease, so a retried attempt claims identically.
    pub fn begin(
        &mut self,
        request: SessionRealizationBeginRequest<'_>,
        now_unix_ms: u64,
    ) -> Result<SessionRealizationBegin, SessionRealizationBeginRejection> {
        let state = &self.aggregate.state;
        transition_environment(
            SessionEnvironmentFacts::project(state, now_unix_ms, None),
            SessionEnvironmentEvent::BeginRealization,
        )
        .map_err(SessionRealizationBeginRejection::Environment)?;
        // Retained execution carries its original generation even when a
        // later replacement is pending. Choose under this same root CAS;
        // never stage a different generation and rely on Worker rejection.
        let activate_pending_resources = match request.expected_resources {
            None => true,
            Some(expected) => {
                if expected.workspace_id != request.owner_scope {
                    return Err(SessionRealizationBeginRejection::ResourceWorkspaceMismatch);
                }
                if state.resources.retained_generation(expected.revision)
                    != Some(&expected.resources)
                {
                    return Err(SessionRealizationBeginRejection::ResourceGenerationNotRetained);
                }
                expected.revision != state.resources.active_revision()
            }
        };
        let current = state.realization.as_ref();
        let assignment = session_realization_assignment(
            current.map(|lease| lease.epoch),
            current.is_some_and(|lease| {
                realization_lease_is_live_at(lease.expires_at_unix_ms, now_unix_ms)
            }),
            current.is_some_and(|lease| lease.owner == request.owner),
            current.is_some_and(|lease| lease.runtime_incarnation == request.runtime_incarnation),
            request.reassign_existing_lease,
        );
        // One authenticated logical owner may immediately fence its prior
        // process incarnation after restart. A different owner can do so
        // only when the claim-authenticated topology edge explicitly asks
        // Control to reassign this otherwise independent Session lease.
        let (lease, assigned) = match (assignment, current) {
            (SessionRealizationAssignment::StaleOwnership, _) => {
                return Err(SessionRealizationBeginRejection::StaleOwnership);
            }
            (SessionRealizationAssignment::EpochExhausted, _) => {
                return Err(SessionRealizationBeginRejection::EpochExhausted);
            }
            (SessionRealizationAssignment::Assign { epoch }, _) => (
                SessionRealizationLease {
                    owner: request.owner.to_owned(),
                    runtime_incarnation: request.runtime_incarnation.to_owned(),
                    epoch,
                    expires_at_unix_ms: request.lease_expires_at_unix_ms,
                },
                true,
            ),
            (SessionRealizationAssignment::Reuse, Some(current)) => (current.clone(), false),
            // Reuse requires a live current lease.
            (SessionRealizationAssignment::Reuse, None) => {
                return Err(SessionRealizationBeginRejection::StaleOwnership);
            }
        };
        let to_claim = state
            .mcp
            .attachments
            .iter()
            .filter(|attachment| match attachment.state {
                McpAttachmentState::Requested => true,
                McpAttachmentState::Realizing | McpAttachmentState::Active => assigned,
                _ => false,
            })
            .map(|attachment| attachment.generation.clone())
            .collect::<Vec<_>>();
        if !assigned && to_claim.is_empty() {
            return Ok(SessionRealizationBegin::Unchanged {
                activate_pending_resources,
            });
        }
        let session_id = state.session_id.clone();
        let counted = assigned
            && matches!(
                state.execution,
                SessionExecutionState::Preparing | SessionExecutionState::Activating
            );
        if activate_pending_resources && state.resources.pending.is_some() {
            self.aggregate
                .resources()
                .start_attempt()
                .map_err(SessionRealizationBeginRejection::Resources)?;
        }
        for generation in to_claim {
            let claim = McpRealizationClaim {
                realization_id: stable_fingerprint(&(
                    &session_id,
                    &generation,
                    &lease.runtime_incarnation,
                    lease.epoch,
                )),
                runtime_incarnation: lease.runtime_incarnation.clone(),
                lease_epoch: lease.epoch,
                lease_expires_at_unix_ms: lease.expires_at_unix_ms,
                stage_idempotency_key: format!(
                    "stage:{}:{}:{}:{}",
                    session_id,
                    generation.attachment_id().0,
                    generation.version(),
                    lease.epoch
                ),
            };
            let mut mcp = self.aggregate.mcp();
            if assigned {
                mcp.claim_recovery(&generation, claim)
            } else {
                mcp.claim_realization(&generation, claim)
            }
            .map_err(SessionRealizationBeginRejection::Mcp)?;
        }
        self.claim(lease, counted)
            .map_err(SessionRealizationBeginRejection::Assignment)?;
        Ok(SessionRealizationBegin::Claimed {
            assigned,
            activate_pending_resources,
        })
    }
}

impl SessionAssignmentOperations<'_> {
    /// Persist the already-admitted initial-preparation request monotonically.
    pub fn request_run_free_preparation(&mut self) -> Result<bool, SessionAggregateCommandError> {
        if self.aggregate.state.is_terminal() || self.aggregate.state.has_active_activities() {
            return Err(SessionAggregateCommandError::PreparationNotReady);
        }
        let previous = self
            .aggregate
            .state
            .realization_progress
            .run_free_initial_preparation;
        self.aggregate
            .state
            .realization_progress
            .run_free_initial_preparation = true;
        Ok(!previous)
    }

    /// Install a new exact successor owner generation. A counted initial
    /// attempt clears its predecessor's failure fields in the same operation.
    pub fn claim(
        &mut self,
        lease: SessionRealizationLease,
        count_initial_attempt: bool,
    ) -> Result<(), SessionAggregateCommandError> {
        let state = &mut self.aggregate.state;
        let expected_epoch = state
            .realization
            .as_ref()
            .map_or(Some(Epoch::FIRST), |old| {
                Epoch::from_persisted(old.epoch).and_then(Epoch::next)
            });
        let same = state
            .realization
            .as_ref()
            .is_some_and(|old| realization_lease_generation_authorizes(&lease, old));
        if lease.owner.trim().is_empty()
            || lease.runtime_incarnation.trim().is_empty()
            || lease.epoch == 0
            || (same && count_initial_attempt)
            || (same
                && state
                    .realization
                    .as_ref()
                    .is_some_and(|old| lease.expires_at_unix_ms < old.expires_at_unix_ms))
        {
            return Err(SessionAggregateCommandError::InvalidLeaseGeneration);
        }
        if !same && expected_epoch.map(Epoch::get) != Some(lease.epoch) {
            return Err(SessionAggregateCommandError::InvalidLeaseGeneration);
        }
        if count_initial_attempt
            && !matches!(
                state.execution,
                SessionExecutionState::Preparing | SessionExecutionState::Activating
            )
        {
            return Err(SessionAggregateCommandError::PreparationNotReady);
        }
        let attempts = if count_initial_attempt {
            state
                .realization_progress
                .attempts
                .checked_add(1)
                .ok_or(SessionAggregateCommandError::AttemptCounterExhausted)?
        } else {
            state.realization_progress.attempts
        };
        if count_initial_attempt {
            state.realization_progress.attempts = attempts;
            state.realization_progress.last_error = None;
            state.realization_progress.failed_at_unix_ms = None;
            state.realization_progress.failure_source_run_id = None;
            state.realization_progress.uncounted_failures = 0;
        }
        state.realization = Some(lease);
        Ok(())
    }

    /// Renew only the selected generation and never regress its expiry.
    pub fn renew(
        &mut self,
        lease: SessionRealizationLease,
    ) -> Result<(), SessionAggregateCommandError> {
        let state = &mut self.aggregate.state;
        let Some(current) = &state.realization else {
            return Err(SessionAggregateCommandError::MissingIncumbentLease);
        };
        if !realization_lease_generation_authorizes(&lease, current)
            || lease.expires_at_unix_ms < current.expires_at_unix_ms
        {
            return Err(SessionAggregateCommandError::InvalidLeaseRenewal);
        }
        if !state.is_terminal() {
            let mut mcp = state.mcp.clone();
            mcp.extend_active_realization_leases(
                &lease.runtime_incarnation,
                lease.epoch,
                lease.expires_at_unix_ms,
            )
            .map_err(|error| SessionAggregateCommandError::ProjectionLease(error.to_string()))?;
            state.mcp = mcp;
        }
        state.realization = Some(lease);
        Ok(())
    }

    /// Extend projections from this root's current selected lease, without
    /// accepting caller-authored owner, epoch, or expiry coordinates.
    pub fn refresh_projection_leases(&mut self) -> Result<(), McpAttachmentError> {
        let state = &mut self.aggregate.state;
        let lease = state
            .realization
            .as_ref()
            .ok_or(McpAttachmentError::StaleRealizationClaim)?;
        let mut next = state.mcp.clone();
        next.extend_active_realization_leases(
            &lease.runtime_incarnation,
            lease.epoch,
            lease.expires_at_unix_ms,
        )?;
        state.mcp = next;
        Ok(())
    }

    /// Record a failed effect without changing the selected lease identity.
    pub fn record_failure(
        &mut self,
        reason: String,
        now: u64,
        source_run_id: Option<awaken_agent_contract::agent::run::Id>,
    ) {
        let progress = &mut self.aggregate.state.realization_progress;
        progress.last_error = Some(reason);
        progress.failed_at_unix_ms = Some(now);
        progress.failure_source_run_id = source_run_id.map(Box::new);
    }

    /// Count one failed effect of an assignment that counted no initial
    /// attempt, so Run-free preparation and Running/Rescheduling recovery
    /// are bounded by the same realization budget as initial realization.
    ///
    /// | execution                 | not-ready | effect                 |
    /// |---------------------------|-----------|------------------------|
    /// | Preparing / Activating    | any       | unchanged (claim counted) |
    /// | any other                 | true      | unchanged (deferral)   |
    /// | any other                 | false     | count + 1              |
    ///
    /// Returns the resulting count; overflow is refused, never wrapped.
    pub fn note_uncounted_failure(
        &mut self,
        not_ready: bool,
    ) -> Result<u32, SessionAggregateCommandError> {
        let state = &mut self.aggregate.state;
        let counted = matches!(
            state.execution,
            SessionExecutionState::Preparing | SessionExecutionState::Activating
        );
        let progress = &mut state.realization_progress;
        if counted || not_ready {
            return Ok(progress.uncounted_failures);
        }
        progress.uncounted_failures = progress
            .uncounted_failures
            .checked_add(1)
            .ok_or(SessionAggregateCommandError::AttemptCounterExhausted)?;
        Ok(progress.uncounted_failures)
    }

    /// Refund exactly one counted capacity refusal, without allowing underflow.
    pub fn refund_capacity_attempt(&mut self) -> Result<(), SessionAggregateCommandError> {
        let progress = &mut self.aggregate.state.realization_progress;
        progress.attempts = progress
            .attempts
            .checked_sub(1)
            .ok_or(SessionAggregateCommandError::MissingCapacityAttempt)?;
        Ok(())
    }

    /// Close the current disposable assignment while retaining its generation.
    pub fn expire(&mut self) {
        if let Some(lease) = &mut self.aggregate.state.realization {
            lease.expires_at_unix_ms = 0;
        }
    }

    /// Compensate only the claimant's exact generation after authority withdrawal.
    pub fn restore_predecessor(
        &mut self,
        attempted: &SessionRealizationLease,
        predecessor: Option<SessionRealizationLease>,
    ) -> Result<(), SessionAggregateCommandError> {
        let state = &mut self.aggregate.state;
        if state.realization.as_ref() != Some(attempted) {
            return Err(SessionAggregateCommandError::StaleCompensation);
        }
        if predecessor
            .as_ref()
            .map_or(Some(Epoch::FIRST), |lease| {
                Epoch::from_persisted(lease.epoch).and_then(Epoch::next)
            })
            .map(Epoch::get)
            != Some(attempted.epoch)
        {
            return Err(SessionAggregateCommandError::InvalidPredecessor);
        }
        state.realization = predecessor;
        Ok(())
    }
}
