// @formal entrypoint session_environment.checkpoint_receipt_requires_every_immutable_generation_axis
// @formal entrypoint session_environment.quiescence_receipt_requires_exact_operation_epoch_mcp_set_and_zero_live_effects
// @formal entrypoint session_environment.restore_receipt_requires_exact_checkpoint_and_nonempty_binding
// @formal entrypoint session_environment.source_disposal_requires_a_verified_checkpoint
//! Recovery-driven Session Environment suspend/restore saga.
//!
//! The Session aggregate decides every step (`transition_environment` over the
//! projected continuation facts); this module only orchestrates: load the
//! root, ask the aggregate for its decision, commit a decided intent or carry
//! out the decided effect through the Runtime, and commit the observation
//! through the Session root CAS before the next external effect.

use awaken_runtime_contract::runs::RunError;
use awaken_session_contract::{
    PersistedSession, SessionAggregate, SessionEnvironmentEvent, SessionEnvironmentFacts,
    SessionEnvironmentInstruction, SessionEnvironmentRejection, SessionEnvironmentStep,
    SessionEnvironmentTransitionError, transition_environment,
};

use std::sync::Arc;

use super::{SessionMutationError, SessionRecoveryCandidates};

fn expected_mcp_generations(
    session: &PersistedSession,
) -> Result<Vec<crate::session_mcp::McpGenerationRef>, SessionContinuationError> {
    session
        .mcp
        .active_generation_refs(&session.session_id)
        .map_err(|error| {
            SessionContinuationError::Runtime(RunError::internal(format!(
                "durable MCP quiescence set is invalid: {error}"
            )))
        })
}

#[derive(Debug, thiserror::Error)]
pub enum SessionContinuationError {
    #[error("Session continuation repository failed: {0}")]
    Repository(String),
    #[error("Session continuation Runtime failed: {0}")]
    Runtime(#[from] RunError),
    #[error("Session continuation transition failed: {0}")]
    Transition(SessionEnvironmentTransitionError),
    #[error("Session continuation is not available for a terminal Session")]
    Terminal,
    #[error("Session continuation did not converge")]
    DidNotConverge,
}

impl SessionContinuationError {
    fn mutation(error: SessionMutationError) -> Self {
        Self::Repository(error.to_string())
    }

    /// The one continuation projection of an Environment decision's refusal:
    /// a terminal Session is terminal, an expired generation and an
    /// unsupported topology keep their classified Runtime errors, and every
    /// other refusal is a transition error.
    fn rejected(rejection: SessionEnvironmentRejection) -> Self {
        match rejection {
            SessionEnvironmentRejection::Terminal => Self::Terminal,
            SessionEnvironmentRejection::GenerationExpired => {
                Self::Runtime(RunError::unavailable_classified(
                    "session_environment_generation_expired",
                    "Resident environment reached its fixed expiry before checkpoint",
                ))
            }
            SessionEnvironmentRejection::UnsupportedContinuationTopology => {
                Self::Runtime(RunError::bad_request(rejection.to_string()))
            }
            rejection => Self::Transition(rejection.into()),
        }
    }

    /// A continuation command never decides materialization or retirement.
    fn not_a_continuation(instruction: SessionEnvironmentInstruction) -> Self {
        Self::Runtime(RunError::internal(format!(
            "a continuation decision instructed {instruction:?}"
        )))
    }

    /// The committed step no longer carries the data its decided effect reads.
    fn step_moved() -> Self {
        Self::rejected(SessionEnvironmentRejection::WrongStep)
    }
}

impl From<SessionEnvironmentTransitionError> for SessionContinuationError {
    fn from(error: SessionEnvironmentTransitionError) -> Self {
        match error {
            SessionEnvironmentTransitionError::Rejected(rejection) => Self::rejected(rejection),
            error => Self::Transition(error),
        }
    }
}

/// The Session Environment suspend/restore saga driven by recovery and by a
/// driving ingress that needs a Resident Environment. It holds the CAS kernel,
/// the Environment and realization-effect runtime roles and the realization
/// component that reprojects a Resident Environment locally.
pub struct SessionContinuations {
    mutations: Arc<crate::SessionMutations>,
    placement: awaken_session_execution_contract::SessionPlacementRoles,
    realizations: Arc<crate::SessionRealizations>,
}

impl SessionContinuations {
    pub(crate) fn new(
        mutations: Arc<crate::SessionMutations>,
        runtime: &crate::SessionExecutionRoles,
        realizations: Arc<crate::SessionRealizations>,
    ) -> Self {
        Self {
            mutations,
            placement: runtime.placement.clone(),
            realizations,
        }
    }

    pub(crate) async fn reconcile_environment_continuations_from(
        &self,
        candidates: &SessionRecoveryCandidates,
        now_unix_ms: u64,
    ) -> usize {
        let mut failures = 0;
        for candidate in &candidates.sessions {
            if !candidate.interests.environment_continuation {
                continue;
            }
            if let Err(error) = self
                .reconcile_environment_continuation(&candidate.session_id, now_unix_ms)
                .await
            {
                failures += 1;
                tracing::warn!(
                    session = %candidate.session_id,
                    error = ?error,
                    "Session Environment continuation remains pending"
                );
            }
        }
        failures
    }

    /// Load the root, let the aggregate decide one continuation command, and
    /// carry out its instruction. A decision that moves the step is an intent
    /// the aggregate records before its effect runs; one that keeps the step
    /// instructs the effect, whose observation the aggregate applies. Either
    /// commits through the root CAS; without an instruction nothing changes.
    async fn continue_environment(
        &self,
        session_id: &awaken_session_types::SessionId,
        event: SessionEnvironmentEvent,
        now_unix_ms: u64,
    ) -> Result<ContinuationStep, SessionContinuationError> {
        let mutations = &self.mutations;
        let owner_scope = mutations
            .owner(session_id)
            .await
            .map_err(SessionContinuationError::mutation)?;
        let mut session = mutations
            .session_aggregate(session_id)
            .await
            .map_err(|error| SessionContinuationError::Repository(error.to_string()))?;
        let decision = transition_environment(
            SessionEnvironmentFacts::project_continuation(session.snapshot(), now_unix_ms),
            event,
        )
        .map_err(SessionContinuationError::rejected)?;
        let Some(instruction) = decision.instruction() else {
            let root = session.into_snapshot();
            return Ok(ContinuationStep {
                owner_scope,
                root,
                advanced: false,
            });
        };
        let reason = match instruction {
            _ if decision.next() == session.environment.step()
                && session.requires_external_environment_owner() =>
            {
                // The committed root/index wakes its registered owner. The
                // Coordinator never invokes local physical effects for it.
                return Ok(ContinuationStep {
                    owner_scope,
                    root: session.into_snapshot(),
                    advanced: false,
                });
            }
            SessionEnvironmentInstruction::Restore
                if decision.next() == session.environment.step() =>
            {
                let tracked = session
                    .environment_materialization()
                    .is_some_and(|facts| facts.tracks_restore_materialization());
                let live = session.realization.as_ref().is_some_and(|lease| {
                    use awaken_time::WallClockExt as _;
                    awaken_session_configuration_contract::resource::realization_lease_is_live_at(
                        lease.expires_at_unix_ms,
                        self.realizations.clock.now_ms(),
                    )
                });
                let unbound = session
                    .environment_materialization()
                    .is_some_and(|facts| facts.target_not_admitted(&session.environment));
                let reassigned = tracked && (unbound || !live);
                if reassigned {
                    use awaken_time::WallClockExt as _;
                    let lease_now = self.realizations.clock.now_ms();
                    let expiry = lease_now
                        .checked_add(crate::realization::LOCAL_SESSION_REALIZATION_LEASE_MS)
                        .ok_or_else(|| {
                            unavailable(
                                "session_environment_restore_assignment_refused",
                                "lease expiry overflow",
                            )
                        })?;
                    session
                        .assignment()
                        .begin(
                            awaken_session_contract::SessionRealizationBeginRequest::new(
                                &owner_scope,
                                self.realizations.local_realization_owner(),
                                self.realizations.runtime_incarnation(),
                                expiry,
                                false,
                                None,
                            ),
                            lease_now,
                        )
                        .map_err(|error| {
                            unavailable("session_environment_restore_assignment_refused", error)
                        })?;
                }
                let admitted = session
                    .environment()
                    .admit_restore_materialization(&owner_scope, now_unix_ms)
                    .map_err(|error| {
                        unavailable("session_environment_restore_materialization_refused", error)
                    })?;
                if admitted || reassigned {
                    "environment-restore-materialization-intent"
                } else {
                    let colocated = self.placement.require_colocated()?;
                    carry_out_effect(
                        colocated,
                        self.realizations.as_ref(),
                        &owner_scope,
                        &mut session,
                        instruction,
                        now_unix_ms,
                    )
                    .await?
                }
            }
            _ if decision.next() == session.environment.step() => {
                let colocated = self.placement.require_colocated()?;
                carry_out_effect(
                    colocated,
                    self.realizations.as_ref(),
                    &owner_scope,
                    &mut session,
                    instruction,
                    now_unix_ms,
                )
                .await?
            }
            SessionEnvironmentInstruction::Quiesce => {
                session
                    .environment()
                    .begin_suspend_at(&owner_scope, now_unix_ms)?;
                "environment-suspend-intent"
            }
            SessionEnvironmentInstruction::Restore => {
                session
                    .environment()
                    .begin_restore(&owner_scope, now_unix_ms)?;
                "environment-restore-intent"
            }
            instruction => return Err(SessionContinuationError::not_a_continuation(instruction)),
        };
        let root = mutations
            .commit_session_snapshot_with_outcome(&owner_scope, session, reason, Vec::new())
            .await
            .map(crate::SnapshotCommit::into_session)
            .map_err(SessionContinuationError::mutation)?;
        Ok(ContinuationStep {
            owner_scope,
            root,
            advanced: true,
        })
    }

    /// Reconcile a due/unfinished suspend operation through its terminal durable
    /// phase. Failures leave the last committed phase for the lifecycle
    /// supervisor to retry.
    pub async fn reconcile_environment_continuation(
        &self,
        session_id: &awaken_session_types::SessionId,
        now_unix_ms: u64,
    ) -> Result<(), SessionContinuationError> {
        for _ in 0..8 {
            let step = self
                .continue_environment(
                    session_id,
                    SessionEnvironmentEvent::AdvanceContinuation,
                    now_unix_ms,
                )
                .await?;
            if !step.advanced {
                return Ok(());
            }
        }
        Err(SessionContinuationError::DidNotConverge)
    }

    /// Driving ingress joins the one committed suspend/restore operation. It
    /// never creates a resume queue and never cancels an ambiguous upload.
    /// A Resident root installs its complete committed projection through
    /// Realization unless an external Worker owns the Environment.
    pub async fn ensure_environment_resident(
        &self,
        session_id: &awaken_session_types::SessionId,
        now_unix_ms: u64,
    ) -> Result<PersistedSession, SessionContinuationError> {
        for _ in 0..12 {
            let ContinuationStep {
                owner_scope,
                root,
                advanced,
            } = self
                .continue_environment(
                    session_id,
                    SessionEnvironmentEvent::RequireResident,
                    now_unix_ms,
                )
                .await?;
            let resident = root.environment.step() == SessionEnvironmentStep::Resident;
            if advanced && !resident {
                continue;
            }
            if resident && !root.requires_external_environment_owner() {
                self.realizations
                    .synchronize_resident_environment_projection(&owner_scope, &root)
                    .await?;
            }
            return Ok(root);
        }
        Err(SessionContinuationError::DidNotConverge)
    }
}

/// One continuation command's outcome: the committed root when an
/// instruction ran (`advanced`), otherwise the unchanged root.
struct ContinuationStep {
    owner_scope: String,
    root: PersistedSession,
    advanced: bool,
}

/// Carry out one decided effect through the Runtime and apply its observation
/// to the candidate; returns the commit reason.
pub(crate) fn apply_continuation_receipt(
    session: &mut SessionAggregate,
    receipt: &awaken_session_execution_contract::SessionContinuationReceipt,
    now: u64,
) -> Result<&'static str, SessionContinuationError> {
    use awaken_session_execution_contract::SessionContinuationReceipt as R;
    match receipt {
        R::Quiesced(receipt) => {
            let expected = expected_mcp_generations(session)?;
            session
                .environment()
                .record_quiescence(receipt, &expected, now)?;
            session
                .mcp()
                .require_reprojection_after_quiescence(&receipt.mcp_generations)
                .map_err(|error| {
                    SessionContinuationError::Repository(format!(
                        "MCP quiescence transition failed: {error}"
                    ))
                })?;
            Ok("environment-quiesced")
        }
        R::Checkpointed(receipt) => {
            session.environment().record_checkpoint(receipt, now)?;
            Ok("environment-checkpointed")
        }
        R::SourceReleasePrepared(receipt) => {
            session
                .environment()
                .record_source_release_prepared(receipt, now)
                .map_err(|e| unavailable("session_environment_source_preparation_stale", e))?;
            Ok("environment-source-release-prepared")
        }
        R::SourceDisposed(receipt) => {
            session.environment().complete_suspend(receipt, now)?;
            Ok("environment-hibernated")
        }
        R::Restored(receipt) => {
            session.environment().complete_restore(receipt, now)?;
            Ok("environment-restored")
        }
        R::ExpiredCheckpointDeleted { checkpoint_id } => {
            if session
                .environment
                .checkpoint()
                .is_none_or(|checkpoint| &checkpoint.id != checkpoint_id)
            {
                return Err(SessionContinuationError::step_moved());
            }
            session.environment().discard_expired_checkpoint(now)?;
            Ok("environment-checkpoint-expired")
        }
    }
}

async fn carry_out_effect(
    roles: &awaken_session_execution_contract::ColocatedRealizationRoles,
    realizations: &crate::SessionRealizations,
    owner_scope: &str,
    session: &mut SessionAggregate,
    instruction: SessionEnvironmentInstruction,
    now: u64,
) -> Result<&'static str, SessionContinuationError> {
    let runtime = roles.environment.as_ref();
    let effects = roles.realization_effects.as_ref();
    if instruction == SessionEnvironmentInstruction::Restore {
        let directive = realizations
            .realization_directive(
                owner_scope.to_owned(),
                session.snapshot(),
                awaken_session_execution_contract::SessionRealizationAction::Continue {
                    instruction,
                },
                false,
            )
            .await
            .map_err(|error| {
                unavailable("session_environment_restore_projection_refused", error)
            })?;
        let request_started = roles.realization.observe_realization_request()?;
        roles
            .realization
            .install_session_projection(
                session.session_id.as_str(),
                directive.projection,
                awaken_session_execution_contract::SessionProjectionInstallMode::Realization {
                    lease: directive.authority.observed(request_started),
                    prepare_session: false,
                },
            )
            .await
            .map_err(|error| SessionContinuationError::Runtime(RunError::from(error)))?;
    }
    let receipt = if instruction == SessionEnvironmentInstruction::Quiesce {
        let (operation, effect, binding, generation) = session
            .environment
            .suspend_source()
            .map(|(op, effect, binding, generation)| {
                (
                    op.clone(),
                    effect.to_owned(),
                    binding.to_owned(),
                    generation.clone(),
                )
            })
            .ok_or_else(SessionContinuationError::step_moved)?;
        let expected = expected_mcp_generations(session)?;
        awaken_session_execution_contract::SessionContinuationReceipt::Quiesced(
            runtime
                .quiesce_session_environment(
                    session.session_id.as_str(),
                    &operation,
                    &effect,
                    &binding,
                    &generation,
                    &expected,
                )
                .await?,
        )
    } else {
        let baseline = session
            .frozen_baseline()
            .ok_or_else(SessionContinuationError::step_moved)?;
        awaken_session_execution_contract::execute_environment_provider_continuation(
            owner_scope,
            &session.session_id,
            &session.environment,
            &baseline.environment.idle_retention,
            session.realization.as_ref(),
            instruction,
            effects,
        )
        .await?
    };
    apply_continuation_receipt(session, &receipt, now)
}

fn unavailable(code: &'static str, error: impl std::fmt::Display) -> SessionContinuationError {
    SessionContinuationError::Runtime(RunError::unavailable_classified(code, error.to_string()))
}

/// Commit one Worker observation through the same continuation receipt laws.
/// The existing Session mutation kernel is the only persistence/CAS authority.
pub(crate) async fn commit_worker_receipt(
    mutations: &crate::SessionMutations,
    command: &awaken_session_execution_contract::AcknowledgeSessionRealization,
    clock: &dyn awaken_time::WallClock,
) -> Result<
    (String, PersistedSession),
    awaken_session_execution_contract::SessionRealizationControlFailure,
> {
    use awaken_session_execution_contract::SessionRealizationControlFailure;
    use awaken_time::WallClockExt as _;
    if !command.published.is_empty() || !command.drained.is_empty() {
        return Err(SessionRealizationControlFailure::Invalid(
            "continuation acknowledgement cannot publish or drain MCP".into(),
        ));
    }
    let receipt = command
        .continuation
        .as_ref()
        .expect("continuation dispatch checked");
    for _ in 0..crate::mutation::ROOT_CAS_ATTEMPTS {
        let owner_scope = mutations
            .owner(&command.session_id)
            .await
            .map_err(crate::realization::mutation_control)?;
        let root = mutations
            .repository()
            .get(&command.session_id)
            .await
            .map_err(crate::realization::repository_control)?;
        if root.is_terminal() {
            return Err(SessionRealizationControlFailure::Terminal);
        }
        let mut session = crate::mutation::aggregate_from_snapshot(root)
            .map_err(crate::realization::repository_control)?;
        crate::realization::verify_lease(&session, &command.lease, clock.now_ms())?;
        let reason =
            crate::continuation::apply_continuation_receipt(&mut session, receipt, clock.now_ms())
                .map_err(crate::realization::unavailable)?;
        let restored = matches!(
            receipt,
            awaken_session_execution_contract::SessionContinuationReceipt::Restored(_)
        );
        if restored {
            // The same root claims reprojection after restoring bytes;
            // no model Run is invented to activate the frozen inputs.
            let lease = session
                .realization
                .clone()
                .ok_or(SessionRealizationControlFailure::NotReady)?;
            session
                .assignment()
                .begin(
                    awaken_session_contract::SessionRealizationBeginRequest::new(
                        &owner_scope,
                        &lease.owner,
                        &lease.runtime_incarnation,
                        lease.expires_at_unix_ms,
                        false,
                        None,
                    ),
                    clock.now_ms(),
                )
                .map_err(|e| crate::realization::begin_refusal(command.session_id.as_str(), e))?;
        }
        match mutations
            .commit_session_snapshot_with_outcome(&owner_scope, session, reason, Vec::new())
            .await
        {
            Ok(commit) => return Ok((owner_scope, commit.into_session())),
            Err(SessionMutationError::Conflict) => continue,
            Err(error) => return Err(crate::realization::unavailable(error)),
        }
    }
    Err(SessionRealizationControlFailure::Conflict)
}

pub(crate) async fn verify_registered_target(
    admission: &crate::SessionExecutionAdmission,
    target: &awaken_session_execution_contract::SessionRealizationTarget,
    session: &PersistedSession,
    now: u64,
) -> Result<(), awaken_session_execution_contract::SessionRealizationControlFailure> {
    use awaken_session_execution_contract::SessionRealizationControlFailure as E;
    let worker = admission
        .worker_for_target(target, now)
        .await?
        .ok_or(E::NotReady)?;
    let mut requirements = admission.placement.requirements(session)?;
    requirements
        .required_capabilities
        .insert(awaken_worker_contract::SESSION_ENVIRONMENT_CONTINUATION_CAPABILITY.to_owned());
    if worker.state != awaken_worker_contract::WorkerState::Ready
        || awaken_worker_contract::can_claim(&worker.manifest, &requirements).is_err()
    {
        return Err(E::NotReady);
    }
    Ok(())
}

/// Project only the continuation action. The Environment FSM still decides it.
pub(crate) fn realization_action(
    session: &PersistedSession,
    now: u64,
) -> Result<
    Option<awaken_session_execution_contract::SessionRealizationAction>,
    awaken_session_execution_contract::SessionRealizationControlFailure,
> {
    use awaken_session_execution_contract::{
        SessionRealizationAction as A, SessionRealizationControlFailure as E,
    };
    if session.environment.admits_realization_effects() {
        return Ok(None);
    }
    if matches!(&session.environment, awaken_session_contract::SessionEnvironmentState::Hibernated { checkpoint, generation } if !checkpoint.expired_at(now) && !generation.expired_at(now))
    {
        return Ok(Some(A::Released));
    }
    let event = if matches!(
        session.environment,
        awaken_session_contract::SessionEnvironmentState::Restoring { .. }
            | awaken_session_contract::SessionEnvironmentState::Hibernated { .. }
    ) {
        SessionEnvironmentEvent::RequireResident
    } else {
        SessionEnvironmentEvent::AdvanceContinuation
    };
    let decision = transition_environment(
        SessionEnvironmentFacts::project_continuation(session, now),
        event,
    )
    .map_err(|e| E::Unavailable(e.to_string()))?;
    let instruction = decision.instruction().ok_or(E::NotReady)?;
    Ok(Some(A::Continue { instruction }))
}
