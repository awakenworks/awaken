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

use awaken_session_execution_contract::EnvironmentRuntime;
use awaken_session_state_contract::{
    PersistedSession, SessionAggregate, SessionEnvironmentEvent, SessionEnvironmentFacts,
    SessionEnvironmentInstruction, SessionEnvironmentRejection, SessionEnvironmentStep,
    SessionEnvironmentTransitionError, transition_environment,
};
use awaken_session_types::RunError;

use super::{SessionApplication, SessionMutationError, SessionRecoveryCandidates};

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

impl SessionApplication {
    pub(super) async fn reconcile_environment_continuations_from(
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
        let environment = self.runtime().environment();
        let reason = match instruction {
            _ if decision.next() == session.environment.step() => {
                carry_out_effect(
                    environment,
                    self.runtime().realization_effects(),
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
                self.synchronize_resident_environment_projection(&owner_scope, &root)
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
async fn carry_out_effect(
    runtime: &dyn EnvironmentRuntime,
    effects: &dyn awaken_session_execution_contract::RealizationEffects,
    owner_scope: &str,
    session: &mut SessionAggregate,
    instruction: SessionEnvironmentInstruction,
    now: u64,
) -> Result<&'static str, SessionContinuationError> {
    let session_id = session.session_id.clone();
    let thread = session_id.as_str();
    let suspend_source = || {
        session
            .environment
            .suspend_source()
            .map(|(operation, effect, binding, generation)| {
                (
                    operation.clone(),
                    effect.to_owned(),
                    binding.to_owned(),
                    generation.clone(),
                )
            })
            .ok_or_else(SessionContinuationError::step_moved)
    };
    match instruction {
        SessionEnvironmentInstruction::Quiesce => {
            let (operation, source_effect_id, source_binding, generation) = suspend_source()?;
            let expected = expected_mcp_generations(session)?;
            let receipt = runtime
                .quiesce_session_environment(
                    thread,
                    &operation,
                    &source_effect_id,
                    &source_binding,
                    &generation,
                    &expected,
                )
                .await?;
            session
                .environment()
                .record_quiescence(&receipt, &expected, now)?;
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
        SessionEnvironmentInstruction::Checkpoint => {
            let policy = session
                .frozen_baseline()
                .ok_or_else(|| {
                    SessionContinuationError::Repository("baseline is not frozen".into())
                })?
                .environment
                .idle_retention
                .clone();
            let request = session
                .environment
                .checkpoint_request(owner_scope, &session_id, &policy)?
                .ok_or_else(SessionContinuationError::step_moved)?;
            let bound = std::time::Duration::from_secs(policy.max_checkpoint_duration_secs);
            let receipt = tokio::time::timeout(
                bound,
                effects.checkpoint_session_environment(thread, request),
            )
            .await
            .map_err(|_| {
                SessionContinuationError::Runtime(RunError::unavailable_classified(
                    "session_environment_checkpoint_timeout",
                    "Session environment checkpoint exceeded its frozen duration bound",
                ))
            })??;
            session.environment().record_checkpoint(&receipt, now)?;
            Ok("environment-checkpointed")
        }
        SessionEnvironmentInstruction::PrepareSourceRelease => {
            let (_, _, source_binding, generation) = suspend_source()?;
            let preparation = session
                .source_release_preparation_effect()
                .map_err(|error| {
                    unavailable("session_environment_source_preparation_unauthorized", error)
                })?;
            let receipt = effects
                .prepare_checkpoint_source_disposal(
                    thread,
                    &preparation,
                    &generation,
                    &source_binding,
                )
                .await?;
            session
                .environment()
                .record_source_release_prepared(&receipt, now)
                .map_err(|error| {
                    unavailable("session_environment_source_preparation_stale", error)
                })?;
            Ok("environment-source-release-prepared")
        }
        SessionEnvironmentInstruction::DisposeSource => {
            let disposal = session.source_release_disposal().map_err(|error| {
                unavailable("session_environment_source_disposal_unauthorized", error)
            })?;
            let receipt = effects
                .dispose_prepared_checkpoint_source(thread, &disposal)
                .await?;
            session.environment().complete_suspend(&receipt, now)?;
            Ok("environment-hibernated")
        }
        SessionEnvironmentInstruction::Restore => {
            let request = session
                .environment
                .restoring_request(owner_scope, &session_id)
                .ok_or_else(SessionContinuationError::step_moved)?;
            let receipt = effects
                .restore_checkpointed_session_environment(request)
                .await?;
            session.environment().complete_restore(&receipt, now)?;
            Ok("environment-restored")
        }
        // Expiry authorizes a fresh Sandbox from the frozen Environment. The
        // idempotent delete precedes the CAS, so a failed delete keeps the
        // reference for retry and a crash after it replays the same delete.
        SessionEnvironmentInstruction::DeleteExpiredCheckpoint => {
            let checkpoint = session
                .environment
                .checkpoint()
                .cloned()
                .ok_or_else(SessionContinuationError::step_moved)?;
            effects
                .delete_session_checkpoint(thread, &checkpoint)
                .await?;
            session.environment().discard_expired_checkpoint(now)?;
            Ok("environment-checkpoint-expired")
        }
        SessionEnvironmentInstruction::Materialize(_) | SessionEnvironmentInstruction::Retire => {
            Err(SessionContinuationError::not_a_continuation(instruction))
        }
    }
}

fn unavailable(code: &'static str, error: impl std::fmt::Display) -> SessionContinuationError {
    SessionContinuationError::Runtime(RunError::unavailable_classified(code, error.to_string()))
}
