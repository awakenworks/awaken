//! Recovery-driven Session Environment suspend/restore saga.
//!
//! Every phase transition commits through the Session root CAS before the next
//! external effect. The Runtime owns quiescence and provider I/O; this module
//! owns only durable orchestration and retries.

use awaken_session_contract::{
    EnvironmentIdleRetentionMode, PersistedSession, RunError, SessionEnvironmentState,
    SessionEnvironmentTransitionError, SessionExecutionState, SuspendPhase,
};

use super::realization::repository_control;
use super::{
    SessionApplication, SessionMutationError, SessionRecoveryCandidates,
    validate_environment_continuation_topology,
};

fn expected_mcp_generations(
    session: &PersistedSession,
) -> Result<Vec<awaken_session_contract::McpGenerationRef>, SessionContinuationError> {
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
    Transition(#[from] SessionEnvironmentTransitionError),
    #[error("Session continuation Memory reconciliation failed: {0}")]
    Memory(#[from] awaken_session_contract::SessionMemoryReconciliationError),
    #[error("Session continuation is not available for a terminal Session")]
    Terminal,
    #[error("Session continuation did not converge")]
    DidNotConverge,
}

fn input_is_memory(input: &awaken_session_contract::ResolvedInput) -> bool {
    matches!(
        &input.source,
        awaken_session_contract::ResolvedInputSource::MemoryStore { .. }
    )
}

/// Validate the aggregate's current source binding before committing a suspend
/// intent. A pending Memory generation has no single physical input/evidence
/// join and therefore remains resident. Active Memory reuses the same neutral
/// None/Some validator that source disposal invokes on historical rows.
fn validate_suspend_memory_preflight(
    session: &PersistedSession,
) -> Result<(), awaken_session_contract::SessionMemoryReconciliationError> {
    if session
        .resources
        .pending
        .as_ref()
        .is_some_and(|pending| pending.inputs().iter().any(input_is_memory))
    {
        return Err(awaken_session_contract::SessionMemoryReconciliationError::ResourceMismatch);
    }
    let active_inputs = session.resources.active.inputs();
    if !active_inputs.iter().any(input_is_memory) {
        return Ok(());
    }
    let binding = session
        .environment
        .binding()
        .ok_or(awaken_session_contract::SessionMemoryReconciliationError::EnvironmentMismatch)?;
    let handle = serde_json::from_str::<awaken_provisioning_contract::SandboxHandle>(binding)
        .map_err(|error| {
            awaken_session_contract::SessionMemoryReconciliationError::InvalidIntent(
                error.to_string(),
            )
        })?;
    let materializations = handle.memory_materializations().map_err(|error| {
        awaken_session_contract::SessionMemoryReconciliationError::InvalidIntent(error.to_string())
    })?;
    awaken_session_contract::validate_continuation_memory_materializations(
        active_inputs,
        materializations,
    )?;
    Ok(())
}

impl SessionContinuationError {
    fn mutation(error: SessionMutationError) -> Self {
        Self::Repository(error.to_string())
    }
}

impl SessionApplication {
    pub(crate) async fn authorize_checkpoint_release_artifact_effect_from_root(
        &self,
        session_id: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
    ) -> Result<String, awaken_session_contract::SessionRealizationControlFailure> {
        let session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_control)?;
        session
            .authorize_checkpoint_release_artifact_effect(operation)
            .map_err(|error| match error {
                awaken_session_contract::SessionEnvironmentReceiptError::RealizationStale
                | awaken_session_contract::SessionEnvironmentReceiptError::Mismatch => {
                    awaken_session_contract::SessionRealizationControlFailure::StaleOwnership
                }
                awaken_session_contract::SessionEnvironmentReceiptError::WrongPhase
                    if session.is_terminal() =>
                {
                    awaken_session_contract::SessionRealizationControlFailure::Terminal
                }
                awaken_session_contract::SessionEnvironmentReceiptError::WrongPhase => {
                    awaken_session_contract::SessionRealizationControlFailure::NotReady
                }
                error => awaken_session_contract::SessionRealizationControlFailure::Invalid(
                    error.to_string(),
                ),
            })?;
        self.session_repository()
            .owner(session_id)
            .await
            .map_err(repository_control)
    }

    pub(super) async fn reconcile_environment_continuations_from(
        &self,
        candidates: &SessionRecoveryCandidates,
        now_unix_ms: u64,
    ) -> usize {
        let mut failures = 0;
        for candidate in &candidates.sessions {
            let session = match self.session_repository().get(&candidate.session_id).await {
                Ok(session) => session,
                Err(awaken_session_contract::SessionRepositoryError::NotFound) => continue,
                Err(error) => {
                    failures += 1;
                    tracing::warn!(
                        session = %candidate.session_id,
                        error = ?error,
                        "Session Environment continuation reload failed"
                    );
                    continue;
                }
            };
            if session.is_terminal() {
                continue;
            }
            if let Err(error) = self
                .reconcile_environment_continuation(&session.session_id, now_unix_ms)
                .await
            {
                failures += 1;
                tracing::warn!(
                    session = %session.session_id,
                    error = ?error,
                    "Session Environment continuation remains pending"
                );
            }
        }
        failures
    }

    async fn commit_continuation(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        reason: &str,
    ) -> Result<PersistedSession, SessionContinuationError> {
        self.commit_session_snapshot(owner_scope, session, reason, Vec::new())
            .await
            .map_err(SessionContinuationError::mutation)
    }

    /// Drive at most one durable phase/effect. `Ok(false)` means no continuation
    /// work is currently due; a caller may safely back off.
    async fn advance_environment_continuation(
        &self,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Result<bool, SessionContinuationError> {
        let owner_scope = self
            .owner(session_id)
            .await
            .map_err(SessionContinuationError::mutation)?;
        let mut session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(|error| SessionContinuationError::Repository(error.to_string()))?;
        if session.is_terminal() {
            return Err(SessionContinuationError::Terminal);
        }
        let baseline = session
            .frozen_baseline()
            .ok_or_else(|| SessionContinuationError::Repository("baseline is not frozen".into()))?
            .clone();
        validate_environment_continuation_topology(
            baseline.runtime_placement,
            &baseline.environment.idle_retention,
        )?;

        match session.environment.clone() {
            SessionEnvironmentState::Resident {
                generation: Some(_),
                idle_since_unix_ms: Some(idle_since),
                ..
            } if session.execution == SessionExecutionState::Idle
                && baseline.environment.idle_retention.mode
                    == EnvironmentIdleRetentionMode::CheckpointAndRelease
                && now_unix_ms
                    >= idle_since.saturating_add(
                        baseline
                            .environment
                            .idle_retention
                            .checkpoint_after_secs
                            .saturating_mul(1_000),
                    ) =>
            {
                validate_suspend_memory_preflight(&session)?;
                session.environment.begin_suspend_at(
                    &owner_scope,
                    session_id,
                    session.activity_epoch,
                    session.realization.clone(),
                    now_unix_ms,
                )?;
                self.commit_continuation(&owner_scope, session, "environment-suspend-intent")
                    .await?;
                Ok(true)
            }
            SessionEnvironmentState::Suspending {
                operation,
                source_effect_id,
                source_binding,
                generation,
                suspend_phase: SuspendPhase::Quiescing,
                ..
            } => {
                let expected_mcp_generations = expected_mcp_generations(&session)?;
                let receipt = self
                    .runtime()
                    .quiesce_session_environment(
                        session_id,
                        &operation,
                        &source_effect_id,
                        &source_binding,
                        &generation,
                        &expected_mcp_generations,
                    )
                    .await?;
                session
                    .environment
                    .record_quiescence(&receipt, &expected_mcp_generations)?;
                session
                    .mcp
                    .require_reprojection_after_quiescence(session_id, &receipt.mcp_generations)
                    .map_err(|error| {
                        SessionContinuationError::Repository(format!(
                            "MCP quiescence transition failed: {error}"
                        ))
                    })?;
                self.commit_continuation(&owner_scope, session, "environment-quiesced")
                    .await?;
                Ok(true)
            }
            SessionEnvironmentState::Suspending {
                generation,
                suspend_phase: SuspendPhase::Uploading,
                ..
            } => {
                if generation.expired_at(now_unix_ms) {
                    return Err(SessionContinuationError::Runtime(
                        RunError::unavailable_classified(
                            "session_environment_generation_expired",
                            "Resident environment reached its fixed expiry before checkpoint",
                        ),
                    ));
                }
                let policy = &baseline.environment.idle_retention;
                let request = session
                    .environment
                    .checkpoint_request(&owner_scope, session_id, policy)?
                    .ok_or(SessionEnvironmentTransitionError::WrongPhase)?;
                let receipt = tokio::time::timeout(
                    std::time::Duration::from_secs(policy.max_checkpoint_duration_secs),
                    self.runtime()
                        .checkpoint_session_environment(session_id, request),
                )
                .await
                .map_err(|_| {
                    SessionContinuationError::Runtime(RunError::unavailable_classified(
                        "session_environment_checkpoint_timeout",
                        "Session environment checkpoint exceeded its frozen duration bound",
                    ))
                })??;
                session.environment.record_checkpoint(&receipt)?;
                self.commit_continuation(&owner_scope, session, "environment-checkpointed")
                    .await?;
                Ok(true)
            }
            SessionEnvironmentState::Suspending {
                source_binding,
                generation,
                suspend_phase: SuspendPhase::ReadyToDispose,
                checkpoint: Some(_),
                source_release_preparation: None,
                ..
            } => {
                let preparation = session
                    .source_release_preparation_effect()
                    .map_err(|error| {
                        SessionContinuationError::Runtime(RunError::unavailable_classified(
                            "session_environment_source_preparation_unauthorized",
                            error.to_string(),
                        ))
                    })?;
                let receipt = self
                    .runtime()
                    .prepare_checkpoint_source_disposal(
                        session_id,
                        &preparation,
                        &generation,
                        &source_binding,
                    )
                    .await?;
                session
                    .record_source_release_prepared(&receipt)
                    .map_err(|error| {
                        SessionContinuationError::Runtime(RunError::unavailable_classified(
                            "session_environment_source_preparation_stale",
                            error.to_string(),
                        ))
                    })?;
                self.commit_continuation(
                    &owner_scope,
                    session,
                    "environment-source-release-prepared",
                )
                .await?;
                Ok(true)
            }
            SessionEnvironmentState::Suspending {
                suspend_phase: SuspendPhase::Disposing,
                checkpoint: Some(_),
                source_release_preparation: Some(_),
                ..
            } => {
                let disposal = session.source_release_disposal().map_err(|error| {
                    SessionContinuationError::Runtime(RunError::unavailable_classified(
                        "session_environment_source_disposal_unauthorized",
                        error.to_string(),
                    ))
                })?;
                let receipt = self
                    .runtime()
                    .dispose_prepared_checkpoint_source(session_id, &disposal)
                    .await?;
                session.environment.complete_suspend(&receipt)?;
                self.commit_continuation(&owner_scope, session, "environment-hibernated")
                    .await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Reconcile a due/unfinished suspend operation through its terminal durable
    /// phase. Failures leave the last committed phase for the lifecycle
    /// supervisor to retry.
    pub async fn reconcile_environment_continuation(
        &self,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Result<(), SessionContinuationError> {
        for _ in 0..8 {
            if !self
                .advance_environment_continuation(session_id, now_unix_ms)
                .await?
            {
                return Ok(());
            }
        }
        Err(SessionContinuationError::DidNotConverge)
    }

    /// Driving ingress joins the one committed suspend/restore operation. It
    /// never creates a resume queue and never cancels an ambiguous upload.
    pub async fn ensure_environment_resident(
        &self,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Result<PersistedSession, SessionContinuationError> {
        for _ in 0..12 {
            let owner_scope = self
                .owner(session_id)
                .await
                .map_err(SessionContinuationError::mutation)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(|error| SessionContinuationError::Repository(error.to_string()))?;
            if session.is_terminal() {
                return Err(SessionContinuationError::Terminal);
            }
            if let Some(baseline) = session.frozen_baseline() {
                validate_environment_continuation_topology(
                    baseline.runtime_placement,
                    &baseline.environment.idle_retention,
                )?;
            }
            match session.environment.clone() {
                SessionEnvironmentState::Unmaterialized => return Ok(session),
                SessionEnvironmentState::Resident { .. } => {
                    if !self.requires_external_realization(&session) {
                        self.synchronize_resident_environment_projection(&owner_scope, &session)
                            .await?;
                    }
                    return Ok(session);
                }
                SessionEnvironmentState::Suspending { .. } => {
                    self.reconcile_environment_continuation(session_id, now_unix_ms)
                        .await?;
                }
                SessionEnvironmentState::Hibernated {
                    checkpoint,
                    generation,
                } if checkpoint.expired_at(now_unix_ms) || generation.expired_at(now_unix_ms) => {
                    // Expiry authorizes a fresh Sandbox from the already-frozen
                    // Environment. Delete is idempotent and happens first so a
                    // failed delete retains the reference for retry; a crash after
                    // delete replays the same delete before the CAS.
                    self.runtime()
                        .delete_session_checkpoint(session_id, &checkpoint)
                        .await?;
                    session.environment = SessionEnvironmentState::Unmaterialized;
                    self.commit_continuation(
                        &owner_scope,
                        session,
                        "environment-checkpoint-expired",
                    )
                    .await?;
                }
                SessionEnvironmentState::Hibernated { .. } => {
                    let next_epoch = session.activity_epoch.checked_add(1).ok_or_else(|| {
                        SessionContinuationError::Repository("activity epoch exhausted".into())
                    })?;
                    session.environment.begin_restore(
                        &owner_scope,
                        session_id,
                        next_epoch,
                        session.realization.clone(),
                        now_unix_ms,
                    )?;
                    self.commit_continuation(&owner_scope, session, "environment-restore-intent")
                        .await?;
                }
                SessionEnvironmentState::Restoring { .. } => {
                    let request = session
                        .environment
                        .restoring_request(&owner_scope, session_id)
                        .ok_or(SessionEnvironmentTransitionError::NotRestoring)?;
                    let receipt = self
                        .runtime()
                        .restore_checkpointed_session_environment(request)
                        .await?;
                    session.environment.complete_restore(&receipt)?;
                    let committed = self
                        .commit_continuation(&owner_scope, session, "environment-restored")
                        .await?;
                    if !self.requires_external_realization(&committed) {
                        self.synchronize_resident_environment_projection(&owner_scope, &committed)
                            .await?;
                    }
                    return Ok(committed);
                }
            }
        }
        Err(SessionContinuationError::DidNotConverge)
    }
}
