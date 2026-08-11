//! Recovery-driven Session Environment suspend/restore saga.
//!
//! Every phase transition commits through the Session root CAS before the next
//! external effect. The Runtime owns quiescence and provider I/O; this module
//! owns only durable orchestration and retries.

use awaken_session_contract::{
    EnvironmentIdleRetentionMode, PersistedSession, RunError, SessionEnvironmentState,
    SessionEnvironmentTransitionError, SessionExecutionState, SuspendPhase,
};

use super::{SessionApplication, SessionMutationError};

#[derive(Debug, thiserror::Error)]
pub enum SessionContinuationError {
    #[error("Session continuation repository failed: {0}")]
    Repository(String),
    #[error("Session continuation Runtime failed: {0}")]
    Runtime(#[from] RunError),
    #[error("Session continuation transition failed: {0}")]
    Transition(#[from] SessionEnvironmentTransitionError),
    #[error("Session continuation is not available for a terminal Session")]
    Terminal,
    #[error("Session continuation did not converge")]
    DidNotConverge,
}

impl SessionContinuationError {
    fn mutation(error: SessionMutationError) -> Self {
        Self::Repository(error.to_string())
    }
}

impl SessionApplication {
    pub(crate) async fn reconcile_environment_continuations(&self, now_unix_ms: u64) -> usize {
        let sessions = match self.session_repository().reconcilable_sessions().await {
            Ok(scan) => scan.sessions,
            Err(error) => {
                tracing::warn!(error = ?error, "Session Environment continuation scan failed");
                return 1;
            }
        };
        let mut failures = 0;
        for scoped in sessions {
            let session = scoped.session;
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
                session.environment.begin_suspend(
                    session_id,
                    session.activity_epoch,
                    session.realization.clone(),
                )?;
                self.commit_continuation(&owner_scope, session, "environment-suspend-intent")
                    .await?;
                Ok(true)
            }
            SessionEnvironmentState::Suspending {
                operation,
                generation,
                suspend_phase: SuspendPhase::Quiescing,
                ..
            } => {
                let receipt = self
                    .runtime()
                    .quiesce_session_environment(session_id, &operation, &generation)
                    .await?;
                session.environment.record_quiescence(&receipt)?;
                self.commit_continuation(&owner_scope, session, "environment-quiesced")
                    .await?;
                Ok(true)
            }
            SessionEnvironmentState::Suspending {
                operation,
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
                let request = awaken_session_contract::SandboxCheckpointRequest {
                    workspace_id: owner_scope.clone(),
                    session_id: session_id.to_string(),
                    operation,
                    generation,
                    format: policy.checkpoint_format.clone(),
                    created_at_unix_ms: now_unix_ms,
                    expires_at_unix_ms: session
                        .environment
                        .generation()
                        .map_or(now_unix_ms, |generation| generation.expires_at_unix_ms),
                    max_bytes: policy.max_checkpoint_bytes,
                };
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
                operation,
                source_binding,
                generation,
                suspend_phase: SuspendPhase::ReadyToDispose,
                checkpoint: Some(_),
            } => {
                let receipt = self
                    .runtime()
                    .dispose_checkpoint_source(session_id, &operation, &generation, &source_binding)
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
            match session.environment.clone() {
                SessionEnvironmentState::Unmaterialized
                | SessionEnvironmentState::Resident { .. } => return Ok(session),
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
                        session_id,
                        next_epoch,
                        session.realization.clone(),
                        now_unix_ms,
                    )?;
                    self.commit_continuation(&owner_scope, session, "environment-restore-intent")
                        .await?;
                }
                SessionEnvironmentState::Restoring {
                    operation,
                    checkpoint,
                    generation,
                } => {
                    let agent_id = session.agent_id().unwrap_or_default().to_string();
                    let receipt = self
                        .runtime()
                        .restore_checkpointed_session_environment(
                            &agent_id,
                            session_id,
                            &operation,
                            &generation,
                            &checkpoint,
                        )
                        .await?;
                    session.environment.complete_restore(&receipt)?;
                    self.commit_continuation(&owner_scope, session, "environment-restored")
                        .await?;
                }
            }
        }
        Err(SessionContinuationError::DidNotConverge)
    }
}
