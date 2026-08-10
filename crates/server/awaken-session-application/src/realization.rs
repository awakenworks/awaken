//! Canonical durable Session realization phase commands.

use std::collections::BTreeSet;

use awaken_session_contract::{
    AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
    FailSessionRealization, ManagedLifecycleFact, McpAttachmentState, McpGenerationRef,
    PersistedSession, RunError, SessionExecutionState, SessionRealizationAction,
    SessionRealizationControl, SessionRealizationControlFailure, SessionRealizationDirective,
    SessionRealizationLease, SessionRepositoryError, SessionRuntime, StageMcpAttachment,
};

use super::{SessionApplication, SessionMutationError};
use crate::projection;

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn initial_idle_fact(owner_scope: &str, session_id: &str) -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: format!("session:{session_id}:created"),
        object_id: session_id.to_string(),
        workspace_id: Some(owner_scope.to_string()),
        event_type: "session.status_idled".to_string(),
        timestamp: i64::try_from(now_unix_ms() / 1_000).unwrap_or(i64::MAX),
    }
}

fn unavailable(error: impl std::fmt::Display) -> SessionRealizationControlFailure {
    SessionRealizationControlFailure::Unavailable(error.to_string())
}

fn repository_control(error: SessionRepositoryError) -> SessionRealizationControlFailure {
    match error {
        SessionRepositoryError::NotFound => SessionRealizationControlFailure::NotFound,
        error => unavailable(error),
    }
}

fn mutation_control(error: SessionMutationError) -> SessionRealizationControlFailure {
    match error {
        SessionMutationError::NotFound => SessionRealizationControlFailure::NotFound,
        error => unavailable(error),
    }
}

fn validate_target(
    command: &BeginSessionRealization,
) -> Result<(), SessionRealizationControlFailure> {
    if command.session_id.trim().is_empty()
        || command.target.owner.trim().is_empty()
        || command.target.runtime_incarnation.trim().is_empty()
        || !awaken_session_contract::realization_lease_is_live_at(
            command.target.lease_expires_at_unix_ms,
            now_unix_ms(),
        )
    {
        return Err(SessionRealizationControlFailure::Invalid(
            "Session id, Runtime owner/incarnation, and a future lease expiry are required".into(),
        ));
    }
    if command.target.renew_existing_lease && command.target.reassign_existing_lease {
        return Err(SessionRealizationControlFailure::Invalid(
            "Session realization renewal and reassignment are mutually exclusive".into(),
        ));
    }
    Ok(())
}

fn verify_lease(
    session: &PersistedSession,
    asserted: &SessionRealizationLease,
) -> Result<(), SessionRealizationControlFailure> {
    if !session.realization.as_ref().is_some_and(|current| {
        awaken_session_contract::realization_lease_authorizes(current, asserted, now_unix_ms())
    }) {
        return Err(SessionRealizationControlFailure::StaleOwnership);
    }
    Ok(())
}

fn exact_generation_key(generation: &McpGenerationRef) -> String {
    awaken_session_contract::stable_fingerprint(generation)
}

fn generation_set_is_renewed_successor(
    current: &[McpGenerationRef],
    asserted: &[McpGenerationRef],
) -> bool {
    current.len() == asserted.len()
        && current.iter().all(|expected| {
            asserted.iter().any(|actual| {
                awaken_session_contract::realization_generation_authorizes(expected, actual)
            })
        })
        && current.iter().any(|expected| {
            asserted.iter().any(|actual| {
                awaken_session_contract::realization_generation_authorizes(expected, actual)
                    && expected.lease_expires_at_unix_ms > actual.lease_expires_at_unix_ms
            })
        })
}

/// Failure while the Session application drives durable realization effects.
#[derive(Debug, thiserror::Error)]
pub enum SessionRealizationError {
    #[error("Session realization control failed: {0}")]
    Control(#[source] SessionRealizationControlFailure),
    #[error("Session realization effect failed: {0}")]
    Effect(#[source] RunError),
    #[error("Session realization did not converge")]
    DidNotConverge,
}

/// One Session recovery failure retained for a later reconciliation pass.
#[derive(Debug)]
pub struct SessionReconciliationFailure {
    pub session_id: String,
    pub message: String,
}

/// Protocol-neutral result of one durable Session recovery scan.
#[derive(Debug, Default)]
pub struct SessionReconciliation {
    pub settled: Vec<PersistedSession>,
    pub failures: Vec<SessionReconciliationFailure>,
    pub quarantined: Vec<awaken_session_contract::SessionRecoveryQuarantine>,
    pub pending: usize,
}

struct LocalProjectionSynchronizer<'a> {
    runtime: &'a dyn SessionRuntime,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionProjectionSynchronizer for LocalProjectionSynchronizer<'_> {
    async fn synchronize_session_projection(
        &self,
        session_id: &str,
        projection: &awaken_session_contract::FrozenSessionProjection,
        lease: &SessionRealizationLease,
        prepare_session: bool,
    ) -> Result<(), RunError> {
        if !prepare_session {
            return Ok(());
        }
        self.runtime
            .install_session_realization_lease(session_id, lease.clone());
        self.runtime.install_expected_environment_binding(
            session_id,
            projection.environment.binding().map(str::to_owned),
        )?;
        self.runtime
            .prepare_session(session_id, projection.session_init())
            .await?;
        if let Some(binding) = projection.environment.binding() {
            self.runtime
                .restore_session_environment(&projection.baseline.agent_id, session_id, binding)
                .await?;
        }
        Ok(())
    }
}

impl SessionApplication {
    async fn reconcile_pending_session_state(&self) -> SessionRecoveryCycle {
        let resources = self.reconcile_resource_activations().await;
        let resource_failure_count = resources.failures.len();
        let pending = resources.pending;
        let quarantined = resources.quarantined.len();
        for failure in resources.failures {
            tracing::warn!(
                session = %failure.session_id,
                error = %failure.message,
                "Session Resource reconciliation remains pending"
            );
        }
        if !resources.settled.is_empty() {
            tracing::info!(
                reconciled_resources = resources.settled.len(),
                "reconciled durable Session Resource activations"
            );
        }
        for isolation in resources.quarantined {
            tracing::error!(
                session = %isolation.session_id,
                reason = %isolation.reason,
                "corrupt durable Session remains quarantined"
            );
        }
        let realizations = self.reconcile_session_realizations().await;
        let realization_failure_count = realizations.failures.len();
        for failure in realizations.failures {
            tracing::warn!(
                session = %failure.session_id,
                error = %failure.message,
                "Session realization reconciliation remains pending"
            );
        }
        if !realizations.settled.is_empty() {
            tracing::info!(
                reconciled_realizations = realizations.settled.len(),
                "reconciled durable Session Runtime projections"
            );
        }
        tracing::info!(
            pending_sessions = pending.max(realizations.pending),
            quarantined_sessions = quarantined.max(realizations.quarantined.len()),
            retryable_failures = resource_failure_count + realization_failure_count,
            "Session recovery scan completed"
        );
        SessionRecoveryCycle {
            retryable_failures: resource_failure_count + realization_failure_count,
        }
    }

    /// Run the sole Session lifecycle supervisor for this application instance.
    /// The process composition owns spawning, cancellation, readiness, and join;
    /// this application owns only the convergence sequence and one-shot fence.
    pub async fn run_lifecycle_supervisor(
        self: std::sync::Arc<Self>,
        cancellation: awaken_runtime_contract::CancellationToken,
    ) -> Result<(), String> {
        if !self.claim_lifecycle_supervisor() {
            return Err("Session lifecycle supervisor was already claimed".into());
        }
        let mut recovery_failure_streak = 0_u32;
        let mut next_recovery = tokio::time::Instant::now();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        let initial_dispatch = self.reconcile_work_dispatches().await;
        for failure in initial_dispatch.failures {
            tracing::warn!(
                session = %failure.session_id,
                environment = %failure.environment_id,
                error = %failure.message,
                "Session WorkQueue dispatch remains pending"
            );
        }
        loop {
            tokio::select! {
                () = cancellation.cancelled() => break,
                _ = tokio::time::sleep_until(next_recovery) => {
                    let cycle = self.reconcile_pending_session_state().await;
                    recovery_failure_streak = if cycle.retryable_failures == 0 {
                        0
                    } else {
                        recovery_failure_streak.saturating_add(1)
                    };
                    next_recovery = tokio::time::Instant::now()
                        + session_recovery_delay(recovery_failure_streak);
                }
                _ = interval.tick() => {
                    if let Err(error) = self
                        .renew_due_session_realizations(now_unix_ms())
                        .await
                    {
                        tracing::warn!(
                            error = ?error,
                            "Session realization lease renewal remains pending"
                        );
                    }
                    let dispatch = self.reconcile_work_dispatches().await;
                    for failure in dispatch.failures {
                        tracing::warn!(
                            session = %failure.session_id,
                            environment = %failure.environment_id,
                            error = %failure.message,
                            "Session WorkQueue dispatch remains pending"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Install frozen dispatch facts without acquiring a local realization
    /// lease. Worker-owned placement crosses this seam before enqueue only; the
    /// claimed Worker remains the sole owner of physical realization effects.
    pub async fn install_dispatch_projection(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<(), SessionRealizationError> {
        let projection = Self::frozen_session_projection(owner_scope.to_string(), session)
            .map_err(|error| {
                SessionRealizationError::Effect(RunError::internal(error.to_string()))
            })?;
        self.runtime()
            .prepare_session(&session.session_id, projection.session_init())
            .await
            .map_err(SessionRealizationError::Effect)
    }

    /// Drive one local Session through the canonical durable realization phase
    /// protocol. Protocol adapters may project the committed result but cannot
    /// perform or reorder these effects.
    pub async fn realize_session(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, SessionRealizationError> {
        let lease_expires_at_unix_ms = now_unix_ms().checked_add(300_000).ok_or_else(|| {
            SessionRealizationError::Effect(RunError::internal("lease expiry overflow"))
        })?;
        let directive = self
            .begin_session_realization(BeginSessionRealization {
                session_id: session_id.to_string(),
                target: awaken_session_contract::SessionRealizationTarget {
                    owner: self.local_realization_owner().to_string(),
                    runtime_incarnation: self.runtime_incarnation().to_string(),
                    lease_expires_at_unix_ms,
                    renew_existing_lease: false,
                    reassign_existing_lease: false,
                },
            })
            .await
            .map_err(SessionRealizationError::Control)?;
        self.drive_local_realization(session_id, directive).await?;
        self.session_repository()
            .get(session_id)
            .await
            .map_err(repository_control)
            .map_err(SessionRealizationError::Control)
    }

    async fn drive_local_realization(
        &self,
        session_id: &str,
        directive: SessionRealizationDirective,
    ) -> Result<(), SessionRealizationError> {
        awaken_session_contract::drive_session_realization(
            session_id,
            self,
            &LocalProjectionSynchronizer {
                runtime: self.runtime(),
            },
            self.mcp_realizer(),
            directive,
        )
        .await
        .map_err(|error| match error {
            awaken_session_contract::SessionRealizationDriveError::Effect(error) => {
                SessionRealizationError::Effect(error)
            }
            awaken_session_contract::SessionRealizationDriveError::Control(error) => {
                SessionRealizationError::Control(error)
            }
            awaken_session_contract::SessionRealizationDriveError::DidNotConverge => {
                SessionRealizationError::DidNotConverge
            }
        })
    }

    /// Renew due local projections through the same root-CAS phase protocol.
    pub async fn renew_due_session_realizations(
        &self,
        now_unix_ms: u64,
    ) -> Result<usize, SessionRealizationError> {
        const RENEW_BEFORE_MS: u64 = 150_000;
        const LEASE_MS: u64 = 300_000;
        let renew_before = now_unix_ms.saturating_add(RENEW_BEFORE_MS);
        let requested_expiry = now_unix_ms.saturating_add(LEASE_MS);
        let sessions = self
            .session_repository()
            .reconcilable_sessions()
            .await
            .map_err(|error| SessionRealizationError::Control(unavailable(error)))?;
        let mut renewed = 0;
        for scoped in sessions.sessions {
            let Some(lease) = scoped.session.realization.clone() else {
                continue;
            };
            if scoped.session.is_hidden()
                || lease.owner != self.local_realization_owner()
                || lease.runtime_incarnation != self.runtime_incarnation()
                || lease.expires_at_unix_ms > renew_before
                || !scoped
                    .session
                    .mcp
                    .attachments
                    .iter()
                    .any(|attachment| attachment.state == McpAttachmentState::Active)
            {
                continue;
            }
            let directive = self
                .begin_session_realization(BeginSessionRealization {
                    session_id: scoped.session.session_id.clone(),
                    target: awaken_session_contract::SessionRealizationTarget {
                        owner: lease.owner,
                        runtime_incarnation: lease.runtime_incarnation,
                        lease_expires_at_unix_ms: requested_expiry,
                        renew_existing_lease: true,
                        reassign_existing_lease: false,
                    },
                })
                .await
                .map_err(SessionRealizationError::Control)?;
            self.drive_local_realization(&scoped.session.session_id, directive)
                .await?;
            renewed += 1;
        }
        Ok(renewed)
    }

    /// Recover the one local Runtime projection for every Session that lost its
    /// process incarnation or has unfinished MCP work. This is deliberately the
    /// same canonical realization driver used by create/update, not a restart-only
    /// environment or MCP path. Terminal and Worker-owned Sessions remain untouched.
    pub async fn reconcile_session_realizations(&self) -> SessionReconciliation {
        let mut report = SessionReconciliation::default();
        let sessions = match self.session_repository().reconcilable_sessions().await {
            Ok(sessions) => sessions,
            Err(error) => {
                report.failures.push(SessionReconciliationFailure {
                    session_id: "<repository>".to_string(),
                    message: error.to_string(),
                });
                return report;
            }
        };
        report.pending = sessions.sessions.len();
        report.quarantined.clone_from(&sessions.quarantined);
        for scoped in sessions.sessions {
            let session = scoped.session;
            let now = now_unix_ms();
            let projection_is_current = session.realization.as_ref().is_some_and(|lease| {
                lease.owner == self.local_realization_owner()
                    && lease.runtime_incarnation == self.runtime_incarnation()
                    && awaken_session_contract::realization_lease_is_live_at(
                        lease.expires_at_unix_ms,
                        now,
                    )
            });
            if session.is_terminal() || self.requires_external_realization(&session) {
                continue;
            }
            // Recovery cause/effect decision table:
            // C1 local nonterminal Session; C2 durable lease names this process;
            // C3 lease is live; C4 MCP has unfinished work. E1 a missing/stale
            // process projection is reassigned and rebuilt once; E2 pending MCP
            // is driven by that same phase protocol; E3 an already-current idle
            // projection is a no-op; E4 terminal/remote placement is untouched.
            //
            // | Rule | C1 | C2+C3 | C4 | Effect |
            // | R1 | yes | no | any | E1 (+E2 when pending) |
            // | R2 | yes | yes | yes | E2 |
            // | R3 | yes | yes | no | E3 |
            // | R4 | no | any | any | E4 |
            if projection_is_current && !session.mcp.needs_reconciliation() {
                continue;
            }
            let session_id = session.session_id.clone();
            match self.realize_session(&session_id).await {
                Ok(session) => report.settled.push(session),
                Err(error) => report.failures.push(SessionReconciliationFailure {
                    session_id,
                    message: error.to_string(),
                }),
            }
        }
        report
    }

    fn realization_stage_requests(
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<Vec<StageMcpAttachment>, SessionRealizationControlFailure> {
        session
            .mcp
            .attachments
            .iter()
            .filter(|attachment| {
                attachment.state == McpAttachmentState::Realizing
                    || (attachment.state == McpAttachmentState::Active
                        && !attachment.publication_acknowledged)
            })
            .map(|attachment| {
                projection::stage_mcp_request(owner_scope, &session.session_id, attachment)
                    .map_err(unavailable)
            })
            .collect()
    }

    fn publication_generations(
        session: &PersistedSession,
    ) -> Result<Vec<McpGenerationRef>, SessionRealizationControlFailure> {
        session
            .mcp
            .attachments
            .iter()
            .filter(|attachment| {
                attachment.state == McpAttachmentState::Active
                    && !attachment.publication_acknowledged
            })
            .map(|attachment| {
                projection::mcp_generation_ref(&session.session_id, attachment).map_err(unavailable)
            })
            .collect()
    }

    fn draining_generations(
        session: &PersistedSession,
    ) -> Result<Vec<McpGenerationRef>, SessionRealizationControlFailure> {
        session
            .mcp
            .attachments
            .iter()
            .filter(|attachment| attachment.state == McpAttachmentState::Draining)
            .map(|attachment| {
                projection::mcp_generation_ref(&session.session_id, attachment).map_err(unavailable)
            })
            .collect()
    }

    fn realization_directive(
        owner_scope: String,
        session: &PersistedSession,
        action: SessionRealizationAction,
        activate_pending_resources: bool,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let projection = if activate_pending_resources {
            SessionApplication::frozen_session_projection(owner_scope, session)
        } else {
            SessionApplication::active_frozen_session_projection(owner_scope, session)
        }
        .map_err(|error| unavailable(error.to_string()))?;
        Ok(SessionRealizationDirective {
            projection,
            lease: session
                .realization
                .clone()
                .ok_or(SessionRealizationControlFailure::NotReady)?,
            action,
        })
    }

    fn next_action(
        owner_scope: String,
        session: &PersistedSession,
        prepare_projection: bool,
        activate_pending_resources: bool,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let stages = Self::realization_stage_requests(&owner_scope, session)?;
        // A new runtime incarnation has no process-local baseline even when the
        // Session has zero Resources/MCP. Synchronize the complete frozen
        // projection on assignment; pending Resources independently require the
        // same idempotent synchronization before their external effects.
        let prepare_session = prepare_projection
            || (activate_pending_resources && session.resources.pending.is_some());
        if prepare_session || !stages.is_empty() {
            return Self::realization_directive(
                owner_scope,
                session,
                SessionRealizationAction::Stage {
                    prepare_session,
                    mcp_stages: stages,
                },
                activate_pending_resources,
            );
        }
        let publish = Self::publication_generations(session)?;
        let drain = Self::draining_generations(session)?;
        let action = if publish.is_empty() && drain.is_empty() {
            SessionRealizationAction::Complete
        } else {
            SessionRealizationAction::Publish { publish, drain }
        };
        Self::realization_directive(owner_scope, session, action, activate_pending_resources)
    }

    async fn session_for_realization(
        &self,
        session_id: &str,
    ) -> Result<(String, PersistedSession), SessionRealizationControlFailure> {
        let owner_scope = self.owner(session_id).await.map_err(mutation_control)?;
        let session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_control)?;
        // A failed initial realization is terminal. Treating its now-empty
        // Stage/Publish sets as `Complete` would allow a retried Run claim to
        // execute after silently dropping the failed MCP/Resource projection.
        if session.is_terminal() {
            return Err(SessionRealizationControlFailure::NotReady);
        }
        if session.frozen_baseline().is_none() {
            return Err(SessionRealizationControlFailure::NotReady);
        }
        let session = self
            .ensure_repository_credentials_pinned(&owner_scope, session)
            .await
            .map_err(unavailable)?;
        Ok((owner_scope, session))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SessionRecoveryCycle {
    retryable_failures: usize,
}

fn session_recovery_delay(failure_streak: u32) -> std::time::Duration {
    const BASE_SECONDS: u64 = 30;
    const MAX_SECONDS: u64 = 300;
    let multiplier = 1_u64.checked_shl(failure_streak.min(4)).unwrap_or(16);
    std::time::Duration::from_secs(
        BASE_SECONDS
            .checked_mul(multiplier)
            .unwrap_or(MAX_SECONDS)
            .min(MAX_SECONDS),
    )
}

#[async_trait::async_trait]
impl SessionRealizationControl for SessionApplication {
    async fn begin_session_realization(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        validate_target(&command)?;
        for attempt in 0..SessionApplication::ROOT_CAS_ATTEMPTS {
            let (owner_scope, mut session) =
                self.session_for_realization(&command.session_id).await?;
            let now = now_unix_ms();
            let existing_live = session.realization.as_ref().is_some_and(|lease| {
                awaken_session_contract::realization_lease_is_live_at(lease.expires_at_unix_ms, now)
            });
            let same_owner = session
                .realization
                .as_ref()
                .is_some_and(|lease| lease.owner == command.target.owner);
            let same_incarnation = session.realization.as_ref().is_some_and(|lease| {
                lease.runtime_incarnation == command.target.runtime_incarnation
            });
            if existing_live && !same_owner && !command.target.reassign_existing_lease {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }

            let requested = session
                .mcp
                .attachments
                .iter()
                .filter(|attachment| attachment.state == McpAttachmentState::Requested)
                .map(|attachment| (attachment.attachment_id.clone(), attachment.generation))
                .collect::<Vec<_>>();
            // One authenticated logical owner may immediately fence its prior
            // process incarnation after restart. A different owner can do so
            // only when the claim-authenticated topology edge explicitly asks
            // Control to reassign this otherwise independent Session lease.
            let needs_assignment = !existing_live
                || !same_incarnation
                || (command.target.reassign_existing_lease && !same_owner);
            let renews_assignment = command.target.renew_existing_lease
                && !needs_assignment
                && session.realization.as_ref().is_some_and(|lease| {
                    command.target.lease_expires_at_unix_ms > lease.expires_at_unix_ms
                });
            if !needs_assignment && !renews_assignment && requested.is_empty() {
                return Self::next_action(
                    owner_scope,
                    &session,
                    false,
                    !command.target.renew_existing_lease,
                );
            }

            let lease = if needs_assignment {
                let epoch = match session.realization.as_ref() {
                    Some(lease) => lease.epoch.checked_add(1).ok_or_else(|| {
                        SessionRealizationControlFailure::Invalid(
                            "Session realization lease epoch is exhausted".into(),
                        )
                    })?,
                    None => 1,
                };
                SessionRealizationLease {
                    owner: command.target.owner.clone(),
                    runtime_incarnation: command.target.runtime_incarnation.clone(),
                    epoch,
                    expires_at_unix_ms: command.target.lease_expires_at_unix_ms,
                }
            } else {
                let mut lease = session
                    .realization
                    .clone()
                    .expect("a live assignment was checked");
                if renews_assignment {
                    lease.expires_at_unix_ms = command.target.lease_expires_at_unix_ms;
                }
                lease
            };
            if session.resources.pending.is_some() && !command.target.renew_existing_lease {
                session.resources.start_attempt().map_err(unavailable)?;
            }
            let to_claim = if needs_assignment {
                session
                    .mcp
                    .attachments
                    .iter()
                    .filter(|attachment| {
                        matches!(
                            attachment.state,
                            McpAttachmentState::Requested
                                | McpAttachmentState::Realizing
                                | McpAttachmentState::Active
                        )
                    })
                    .map(|attachment| (attachment.attachment_id.clone(), attachment.generation))
                    .collect::<Vec<_>>()
            } else {
                requested
            };
            for (attachment_id, generation) in to_claim {
                let realization_id = awaken_session_contract::stable_fingerprint(&(
                    &session.session_id,
                    &attachment_id,
                    generation,
                    &lease.runtime_incarnation,
                    lease.epoch,
                ));
                let claim = awaken_session_contract::McpRealizationClaim {
                    realization_id,
                    runtime_incarnation: lease.runtime_incarnation.clone(),
                    lease_epoch: lease.epoch,
                    lease_expires_at_unix_ms: lease.expires_at_unix_ms,
                    stage_idempotency_key: format!(
                        "stage:{}:{}:{}:{}",
                        session.session_id, attachment_id.0, generation.0, lease.epoch
                    ),
                };
                let result = if needs_assignment {
                    session
                        .mcp
                        .claim_recovery(&attachment_id, generation, claim)
                } else {
                    session
                        .mcp
                        .claim_realization(&attachment_id, generation, claim)
                };
                result.map_err(unavailable)?;
            }
            if renews_assignment {
                session
                    .mcp
                    .renew_active_realizations(
                        &lease.runtime_incarnation,
                        lease.epoch,
                        lease.expires_at_unix_ms,
                    )
                    .map_err(unavailable)?;
            }
            if needs_assignment
                && matches!(
                    session.execution,
                    SessionExecutionState::Preparing | SessionExecutionState::Activating
                )
            {
                session.realization_progress.attempts = session
                    .realization_progress
                    .attempts
                    .checked_add(1)
                    .ok_or_else(|| {
                        SessionRealizationControlFailure::Invalid(
                            "Session realization attempt counter is exhausted".into(),
                        )
                    })?;
                session.realization_progress.last_error = None;
            }
            session.realization = Some(lease);
            match self
                .commit_session_snapshot(
                    &owner_scope,
                    session,
                    "begin-session-realization",
                    Vec::new(),
                )
                .await
            {
                Ok(session) => {
                    return Self::next_action(
                        owner_scope,
                        &session,
                        needs_assignment,
                        !command.target.renew_existing_lease,
                    );
                }
                Err(SessionMutationError::Conflict)
                    if attempt + 1 < SessionApplication::ROOT_CAS_ATTEMPTS =>
                {
                    continue;
                }
                Err(SessionMutationError::Conflict) => {
                    return Err(SessionRealizationControlFailure::Conflict);
                }
                Err(error) => return Err(unavailable(error)),
            }
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    async fn activate_session_realization(
        &self,
        command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let (owner_scope, mut session) = self.session_for_realization(&command.session_id).await?;
        verify_lease(&session, &command.lease)?;
        let expected = Self::realization_stage_requests(&owner_scope, &session)?;
        if expected.len() != command.mcp_receipts.len() {
            return Err(SessionRealizationControlFailure::Invalid(
                "MCP realization receipt set is incomplete or contains extras".into(),
            ));
        }
        let mut receipt_keys = BTreeSet::new();
        for receipt in &command.mcp_receipts {
            if !receipt_keys.insert(exact_generation_key(&receipt.generation)) {
                return Err(SessionRealizationControlFailure::Invalid(
                    "MCP realization receipt set contains a duplicate generation".into(),
                ));
            }
        }
        let expected_generations = expected
            .iter()
            .map(|request| request.generation.clone())
            .collect::<Vec<_>>();
        let receipt_generations = command
            .mcp_receipts
            .iter()
            .map(|receipt| receipt.generation.clone())
            .collect::<Vec<_>>();
        if generation_set_is_renewed_successor(&expected_generations, &receipt_generations)
            && command.mcp_receipts.iter().all(|receipt| {
                expected.iter().any(|request| {
                    awaken_session_contract::realization_generation_authorizes(
                        &request.generation,
                        &receipt.generation,
                    ) && request.realization_id == receipt.realization_id
                        && request.selected_plaintext_holder == receipt.selected_plaintext_holder
                })
            })
        {
            // A heartbeat advanced only the exact lease expiry while this Stage
            // was in flight. The predecessor receipt commits nothing; return the
            // latest Stage so the one driver catches up under current authority.
            return Self::next_action(owner_scope, &session, false, false);
        }
        for receipt in &command.mcp_receipts {
            let request = expected
                .iter()
                .find(|request| request.generation == receipt.generation)
                .ok_or_else(|| {
                    SessionRealizationControlFailure::Invalid(
                        "MCP realization receipt names an unclaimed generation".into(),
                    )
                })?;
            receipt
                .verify(request)
                .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        }
        let prepared_pending_resources = match command.prepared_resource_revision {
            Some(revision)
                if session.resources.pending.is_some()
                    && revision == session.resources.revision =>
            {
                true
            }
            Some(_) if session.resources.pending.is_some() => {
                return Err(SessionRealizationControlFailure::Invalid(
                    "prepared Resource generation does not match the pending Session generation"
                        .into(),
                ));
            }
            Some(_) | None => false,
        };
        let prepared_legacy_resources = if session.resources.pending.is_none()
            && session.resources.activations.is_empty()
            && !session.resources.active.inputs.is_empty()
        {
            match command.prepared_resource_revision {
                Some(revision) if revision == session.resources.revision => true,
                Some(_) => {
                    return Err(SessionRealizationControlFailure::Invalid(
                        "prepared legacy Resource generation does not match the active Session generation"
                            .into(),
                    ));
                }
                None => false,
            }
        } else {
            false
        };
        let needs_activation_commit = prepared_pending_resources
            || prepared_legacy_resources
            || session
                .mcp
                .attachments
                .iter()
                .any(|attachment| attachment.state == McpAttachmentState::Realizing);
        if !needs_activation_commit {
            let publish = Self::publication_generations(&session)?;
            let drain = Self::draining_generations(&session)?;
            return Self::realization_directive(
                owner_scope,
                &session,
                SessionRealizationAction::Publish { publish, drain },
                false,
            );
        }
        if prepared_pending_resources {
            session.resources.commit().map_err(unavailable)?;
        }
        if prepared_legacy_resources {
            session.resources.adopt_legacy_active(&command.session_id);
        }
        for request in expected {
            let attachment = session
                .mcp
                .attachments
                .iter()
                .find(|attachment| {
                    attachment.attachment_id == request.generation.attachment_id
                        && attachment.generation == request.generation.generation
                })
                .ok_or(SessionRealizationControlFailure::NotReady)?;
            if attachment.state == McpAttachmentState::Realizing {
                session
                    .mcp
                    .activate(
                        &request.generation.attachment_id,
                        request.generation.generation,
                        &request.realization_id,
                    )
                    .map_err(unavailable)?;
            }
        }
        // A heartbeat may have extended the aggregate lease while the Runtime
        // was staging a previously admitted request. Promote the newly Active
        // durable claim to that current lease before publication. The returned
        // Stage directive then updates the same process-local projection by its
        // renewal binding; it does not reconnect or reopen credentials.
        let current_lease = session
            .realization
            .clone()
            .ok_or(SessionRealizationControlFailure::NotReady)?;
        let renewed_after_activation = session
            .mcp
            .renew_active_realizations(
                &current_lease.runtime_incarnation,
                current_lease.epoch,
                current_lease.expires_at_unix_ms,
            )
            .map_err(unavailable)?;
        session.mcp.begin_obsolete_drains().map_err(unavailable)?;
        // Initial creation remains non-visible until publication acknowledgement.
        // A hot mutation belongs to an already-idle Session, so keep that lifecycle
        // status while its new generation is unacknowledged; a failed replacement
        // must not turn the established Session into a failed create.
        if session.execution != SessionExecutionState::Idle {
            session
                .transition_execution(SessionExecutionState::Activating)
                .map_err(unavailable)?;
        }
        let session = self
            .commit_resource_snapshot(
                &owner_scope,
                session,
                "activate-session-realization",
                Vec::new(),
            )
            .await
            .map_err(|error| match error {
                SessionMutationError::Conflict => SessionRealizationControlFailure::Conflict,
                error => unavailable(error),
            })?;
        if renewed_after_activation > 0 {
            return Self::next_action(owner_scope, &session, false, false);
        }
        let publish = Self::publication_generations(&session)?;
        let drain = Self::draining_generations(&session)?;
        Self::realization_directive(
            owner_scope,
            &session,
            SessionRealizationAction::Publish { publish, drain },
            false,
        )
    }

    async fn acknowledge_session_realization(
        &self,
        command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let (owner_scope, mut session) = self.session_for_realization(&command.session_id).await?;
        verify_lease(&session, &command.lease)?;
        let expected_publish = Self::publication_generations(&session)?;
        let expected_drain = Self::draining_generations(&session)?;
        let keys = |items: &[McpGenerationRef]| {
            items
                .iter()
                .map(exact_generation_key)
                .collect::<BTreeSet<_>>()
        };
        if keys(&command.published).len() != command.published.len()
            || keys(&command.drained).len() != command.drained.len()
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "publication acknowledgement contains a duplicate generation".into(),
            ));
        }
        let replayed_publish = command.published.iter().all(|generation| {
            session.mcp.attachments.iter().any(|attachment| {
                attachment.state == McpAttachmentState::Active
                    && attachment.publication_acknowledged
                    && projection::mcp_generation_ref(&session.session_id, attachment)
                        .is_ok_and(|current| current == *generation)
            })
        });
        let replayed_drain = command.drained.iter().all(|generation| {
            session.mcp.attachments.iter().any(|attachment| {
                attachment.state == McpAttachmentState::Removed
                    && projection::mcp_generation_ref(&session.session_id, attachment)
                        .is_ok_and(|current| current == *generation)
            })
        });
        if expected_publish.is_empty()
            && expected_drain.is_empty()
            && replayed_publish
            && replayed_drain
            && session.execution == SessionExecutionState::Idle
        {
            return Self::realization_directive(
                owner_scope,
                &session,
                SessionRealizationAction::Complete,
                false,
            );
        }
        let publish_mismatch = keys(&expected_publish) != keys(&command.published);
        let drain_mismatch = keys(&expected_drain) != keys(&command.drained);
        if publish_mismatch
            && !drain_mismatch
            && generation_set_is_renewed_successor(&expected_publish, &command.published)
        {
            // Publication happened under a shorter same-epoch lease while a
            // heartbeat advanced durable authority. Do not acknowledge the old
            // fence and do not fail the Session: return the latest Stage/Publish
            // work to the same canonical driver.
            return Self::next_action(owner_scope, &session, false, false);
        }
        if publish_mismatch || drain_mismatch {
            return Err(SessionRealizationControlFailure::Invalid(
                "publication acknowledgement does not match the durable generation set".into(),
            ));
        }
        for generation in expected_publish {
            let realization_id = session
                .mcp
                .attachments
                .iter()
                .find(|attachment| {
                    attachment.attachment_id == generation.attachment_id
                        && attachment.generation == generation.generation
                })
                .and_then(|attachment| attachment.realization.as_ref())
                .map(|claim| claim.realization_id.clone())
                .ok_or(SessionRealizationControlFailure::NotReady)?;
            session
                .mcp
                .acknowledge_publication(
                    &generation.attachment_id,
                    generation.generation,
                    &realization_id,
                )
                .map_err(unavailable)?;
        }
        for generation in expected_drain {
            session
                .mcp
                .finish_drain(&generation.attachment_id, generation.generation)
                .map_err(unavailable)?;
        }
        let initial_ready =
            session.activity_epoch == 0 && session.execution != SessionExecutionState::Idle;
        session
            .transition_execution(SessionExecutionState::Idle)
            .map_err(unavailable)?;
        let ready_fact =
            initial_ready.then(|| initial_idle_fact(&owner_scope, &command.session_id));
        let session = self
            .commit_session_snapshot(
                &owner_scope,
                session,
                "acknowledge-session-realization",
                ready_fact.iter().cloned().collect(),
            )
            .await
            .map_err(|error| match error {
                SessionMutationError::Conflict => SessionRealizationControlFailure::Conflict,
                error => unavailable(error),
            })?;
        if let Some(fact) = &ready_fact {
            self.emit_lifecycle_fact(
                &fact.id,
                &fact.object_id,
                fact.workspace_id.as_deref(),
                &fact.event_type,
            )
            .await;
        }
        Self::realization_directive(
            owner_scope,
            &session,
            SessionRealizationAction::Complete,
            false,
        )
    }

    async fn fail_session_realization(
        &self,
        command: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure> {
        if command.reason.trim().is_empty() {
            return Err(SessionRealizationControlFailure::Invalid(
                "realization failure reason is empty".into(),
            ));
        }
        // Failure delivery is idempotent even though activation_failed is
        // terminal for every new phase command. Handle that exact replay before
        // the common terminal guard used by begin/activate/acknowledge.
        match self.session_repository().get(&command.session_id).await {
            Ok(session) if session.execution == SessionExecutionState::ActivationFailed => {
                return Ok(());
            }
            Ok(_) | Err(SessionRepositoryError::NotFound) => {}
            Err(error) => return Err(repository_control(error)),
        }
        let (owner_scope, mut session) = self.session_for_realization(&command.session_id).await?;
        verify_lease(&session, &command.lease)?;
        if command.prepared_resource_revision.is_some_and(|revision| {
            session.resources.pending.is_some() && revision != session.resources.revision
        }) {
            return Err(SessionRealizationControlFailure::Invalid(
                "failed Resource generation does not match the pending Session generation".into(),
            ));
        }
        if command.prepared_resource_revision == Some(session.resources.revision)
            && session.resources.pending.is_some()
        {
            session
                .resources
                .note_retryable_failure(command.reason.clone())
                .map_err(unavailable)?;
        }
        session.realization_progress.last_error = Some(command.reason.clone());
        let initial_realization = session.execution != SessionExecutionState::Idle;
        if command.retryable
            && initial_realization
            && session.realization_progress.attempts < self.realization_retry_budget()
        {
            // Retain the exact MCP/Resource generation for recovery, but expire
            // this assignment immediately. The next fenced claim increments the
            // lease epoch and the persisted attempt counter before any effect.
            if let Some(lease) = &mut session.realization {
                lease.expires_at_unix_ms = 0;
            }
            self.commit_session_snapshot(
                &owner_scope,
                session,
                "retry-session-realization",
                Vec::new(),
            )
            .await
            .map_err(|error| match error {
                SessionMutationError::Conflict => SessionRealizationControlFailure::Conflict,
                error => unavailable(error),
            })?;
            return Ok(());
        }
        let realizing = session
            .mcp
            .attachments
            .iter()
            .filter(|attachment| attachment.state == McpAttachmentState::Realizing)
            .map(|attachment| {
                (
                    attachment.attachment_id.clone(),
                    attachment.generation,
                    attachment
                        .realization
                        .as_ref()
                        .map(|claim| claim.realization_id.clone()),
                )
            })
            .collect::<Vec<_>>();
        for (attachment_id, generation, realization_id) in realizing {
            session
                .mcp
                .fail_realization(
                    &attachment_id,
                    generation,
                    realization_id
                        .as_deref()
                        .ok_or(SessionRealizationControlFailure::NotReady)?,
                    command.reason.clone(),
                )
                .map_err(unavailable)?;
        }
        if session.execution != SessionExecutionState::Idle {
            session
                .transition_execution(SessionExecutionState::ActivationFailed)
                .map_err(unavailable)?;
        }
        self.commit_session_snapshot(
            &owner_scope,
            session,
            "fail-session-realization",
            Vec::new(),
        )
        .await
        .map_err(|error| match error {
            SessionMutationError::Conflict => SessionRealizationControlFailure::Conflict,
            error => unavailable(error),
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod recovery_backoff_tests {
    use super::session_recovery_delay;

    #[test]
    fn retry_backoff_follows_the_failure_streak_decision_table() {
        /* Causes: C1 consecutive retryable failure count. Effect: E1 next
         * recovery delay. Rules: R1 C1=0=>30s normal cadence; R2 C1=1=>60s;
         * R3 C1=2=>120s; R4 C1=3=>240s; R5 C1>=4=>300s cap. Quarantine is
         * excluded because it is operator-repair work, not retryable work. */
        let rules = [(0, 30), (1, 60), (2, 120), (3, 240), (4, 300), (99, 300)];
        for (streak, seconds) in rules {
            assert_eq!(session_recovery_delay(streak).as_secs(), seconds);
        }
    }
}
