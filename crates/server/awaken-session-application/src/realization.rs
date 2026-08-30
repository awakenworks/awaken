//! Canonical durable Session realization phase commands.

mod control;
use std::collections::BTreeSet;

use awaken_session_contract::{
    AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
    FailSessionRealization, ManagedLifecycleFact, McpAttachmentState, McpGenerationRef,
    PersistedSession, RenewSessionRealization, RunError, SessionEnvironmentState,
    SessionExecutionState, SessionRealizationAction, SessionRealizationControl,
    SessionRealizationControlFailure, SessionRealizationDirective, SessionRealizationLease,
    SessionRepositoryError, SessionRuntime, SessionTerminalCleanupAssignment, StageMcpAttachment,
};

use super::{SessionApplication, SessionMutationError};
use crate::{
    activity::{now_unix_ms, runtime_interval_fact},
    projection,
};

fn initial_idle_fact(owner_scope: &str, session_id: &str) -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: format!("session:{session_id}:created"),
        object_id: session_id.to_string(),
        workspace_id: Some(owner_scope.to_string()),
        event_type: "session.status_idled".to_string(),
        timestamp: i64::try_from(now_unix_ms() / 1_000).unwrap_or(i64::MAX),
        runtime_interval: None,
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

fn validate_realization_target(
    target: &awaken_session_contract::SessionRealizationTarget,
) -> Result<(), SessionRealizationControlFailure> {
    if target.owner.trim().is_empty()
        || target.runtime_incarnation.trim().is_empty()
        || !awaken_session_contract::realization_lease_is_live_at(
            target.lease_expires_at_unix_ms,
            now_unix_ms(),
        )
    {
        return Err(SessionRealizationControlFailure::Invalid(
            "Runtime owner/incarnation and a future lease expiry are required".into(),
        ));
    }
    Ok(())
}

fn validate_target(
    command: &BeginSessionRealization,
) -> Result<(), SessionRealizationControlFailure> {
    if command.session_id.trim().is_empty() {
        return Err(SessionRealizationControlFailure::Invalid(
            "Session id is required".into(),
        ));
    }
    validate_realization_target(&command.target)
}

fn terminal_cleanup_claim_needs_assignment(
    current: Option<&SessionRealizationLease>,
    target: &awaken_session_contract::SessionRealizationTarget,
    now_unix_ms: u64,
) -> bool {
    let Some(current) = current else {
        return true;
    };
    if !awaken_session_contract::realization_lease_is_live_at(
        current.expires_at_unix_ms,
        now_unix_ms,
    ) {
        return true;
    }
    // A restarted process in the same logical Worker slot may immediately
    // fence its predecessor. The exact incarnation may retry only when a later
    // heartbeat supplies a strictly newer expiry; after that CAS, the same
    // heartbeat target is equal and skipped so one faulted Session cannot
    // monopolize its bounded claim loop. A different live owner retains its
    // authority until expiry because cleanup has no Run claim to prove a
    // cross-owner topology takeover.
    current.owner == target.owner
        && (current.runtime_incarnation != target.runtime_incarnation
            || current.expires_at_unix_ms < target.lease_expires_at_unix_ms)
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

/// One phase gate owns every nonterminal realization entry point. Continuation
/// phases have closed Environment admission and must not stage, publish, or
/// drain process-local effects until restore commits Resident again.
fn environment_admits_realization_effects(environment: &SessionEnvironmentState) -> bool {
    matches!(
        environment,
        SessionEnvironmentState::Unmaterialized | SessionEnvironmentState::Resident { .. }
    )
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
        self.runtime
            .install_session_projection(
                session_id,
                projection.clone(),
                awaken_session_contract::SessionProjectionInstallMode::Realization {
                    lease: lease.clone(),
                    prepare_session,
                },
            )
            .await
    }
}

impl SessionApplication {
    pub(crate) async fn reconcile_pending_session_state(
        self: std::sync::Arc<Self>,
    ) -> SessionRecoveryCycle {
        let candidates = match crate::scan_all_reconcilable_sessions(self.session_repository()).await {
            Ok(scan) => super::SessionRecoveryCandidates::from(scan),
            Err(error) => {
                tracing::warn!(error = ?error, "Session recovery candidate scan failed");
                return SessionRecoveryCycle {
                    retryable_failures: 1,
                };
            }
        };
        let resources = self.reconcile_resource_activations_from(&candidates).await;
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
        let continuation_failure_count = self
            .reconcile_environment_continuations_from(&candidates, now_unix_ms())
            .await;
        let realizations = self.reconcile_session_realizations_from(&candidates).await;
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
        // Event reconciliation may enter the complete Runtime Run state machine.
        // Start that phase at a scheduler boundary instead of nesting it below
        // Resource, continuation, and realization recovery. The `JoinSet` keeps
        // the child structurally owned and aborts it if this recovery cycle is
        // cancelled; only its polling stack changes.
        let application = std::sync::Arc::clone(&self);
        let mut event_task = tokio::task::JoinSet::new();
        let event_candidates = candidates.clone();
        event_task.spawn(async move {
            application
                .reconcile_event_batches_from(&event_candidates)
                .await
        });
        let event_batches = match event_task.join_next().await {
            Some(Ok(report)) => report,
            Some(Err(error)) => {
                let mut report = crate::event_batches::EventBatchReconciliation::default();
                report.failures.push((
                    "<supervisor>".to_string(),
                    format!("Session Event recovery task failed: {error}"),
                ));
                report
            }
            None => {
                let mut report = crate::event_batches::EventBatchReconciliation::default();
                report.failures.push((
                    "<supervisor>".to_string(),
                    "Session Event recovery task disappeared".to_string(),
                ));
                report
            }
        };
        let event_batch_failure_count = event_batches.failures.len();
        for (session_id, error) in event_batches.failures {
            tracing::warn!(
                session = %session_id,
                %error,
                "Session Event reconciliation remains pending"
            );
        }
        if event_batches.settled != 0 {
            tracing::info!(
                reconciled_event_batches = event_batches.settled,
                "reconciled durable Session Event batches"
            );
        }
        let outcomes = self.reconcile_outcome_continuations_from(&candidates).await;
        let outcome_failure_count = outcomes.failures.len();
        for (session_id, error) in outcomes.failures {
            tracing::warn!(
                session = %session_id,
                %error,
                "Thread Outcome reconciliation remains pending"
            );
        }
        if outcomes.settled != 0 {
            tracing::info!(
                reconciled_outcomes = outcomes.settled,
                "reconciled durable Thread Outcomes"
            );
        }
        let final_scan_failure_count = match self
            .refresh_event_batch_cutover_validation(event_batch_failure_count)
            .await
        {
            Ok(snapshot) => {
                tracing::info!(
                    validation_generation = snapshot.generation,
                    terminal_with_incomplete_event_batches =
                        snapshot.terminal_with_incomplete_event_batches,
                    event_batch_failures = snapshot.event_batch_failures,
                    quarantined_sessions = snapshot.quarantined,
                    restoring_sessions = snapshot.restoring_sessions,
                    "Session Event-batch cutover validation scan completed"
                );
                0
            }
            Err(error) => {
                tracing::warn!(
                    error = ?error,
                    "Session Event-batch cutover validation scan remains pending"
                );
                1
            }
        };
        let retryable_failures = resource_failure_count
            + continuation_failure_count
            + realization_failure_count
            + event_batch_failure_count
            + outcome_failure_count
            + final_scan_failure_count;
        tracing::info!(
            pending_sessions = pending
                .max(realizations.pending)
                .max(event_batches.pending)
                .max(outcomes.pending),
            quarantined_sessions = quarantined
                .max(realizations.quarantined.len())
                .max(event_batches.quarantined)
                .max(outcomes.quarantined),
            retryable_failures,
            "Session recovery scan completed"
        );
        SessionRecoveryCycle { retryable_failures }
    }

    /// Run the sole Session lifecycle supervisor for this application instance.
    /// The process startup owns spawning, cancellation, readiness, and join;
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
            let recovery_due = tokio::select! {
                () = cancellation.cancelled() => break,
                _ = self.lifecycle_wakeup.notified() => true,
                _ = tokio::time::sleep_until(next_recovery) => true,
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
                    false
                }
            };
            if !recovery_due {
                continue;
            }

            // The sole recovery cycle composes Resource, realization,
            // Event-batch, and Outcome reconciliation. Await it as a distinct
            // Tokio task so its first poll begins at the scheduler boundary;
            // nesting the complete state machine in this supervisor can exceed
            // a default worker stack before the first User batch is admitted.
            let application = std::sync::Arc::clone(&self);
            let mut recovery_task = tokio::task::JoinSet::new();
            recovery_task.spawn(async move { application.reconcile_pending_session_state().await });
            let cycle = recovery_task
                .join_next()
                .await
                .ok_or_else(|| "Session recovery task disappeared".to_string())?
                .map_err(|error| format!("Session recovery task failed: {error}"))?;
            recovery_failure_streak = if cycle.retryable_failures == 0 {
                0
            } else {
                recovery_failure_streak.saturating_add(1)
            };
            next_recovery =
                tokio::time::Instant::now() + session_recovery_delay(recovery_failure_streak);
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
        let projection = self
            .frozen_session_projection(owner_scope.to_string(), session, true)
            .await
            .map_err(|error| {
                SessionRealizationError::Effect(RunError::internal(error.to_string()))
            })?;
        self.runtime()
            .install_session_projection(
                &session.session_id,
                projection,
                awaken_session_contract::SessionProjectionInstallMode::Dispatch,
            )
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
        self.refresh_executable_projections()
            .await
            .map_err(|error| SessionRealizationError::Control(unavailable(error)))?;
        self.realize_session_after_refresh(session_id).await
    }

    /// Continue realization after the owning application operation refreshed
    /// every executable projection before its first catalog read or mutation.
    pub(super) async fn realize_session_after_refresh(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, SessionRealizationError> {
        let lease_expires_at_unix_ms = now_unix_ms().checked_add(300_000).ok_or_else(|| {
            SessionRealizationError::Effect(RunError::internal("lease expiry overflow"))
        })?;
        let directive = self
            .begin_session_realization_after_refresh(BeginSessionRealization {
                session_id: session_id.to_string(),
                target: awaken_session_contract::SessionRealizationTarget {
                    owner: self.local_realization_owner().to_string(),
                    runtime_incarnation: self.runtime_incarnation().to_string(),
                    lease_expires_at_unix_ms,
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
        let control = RefreshedSessionRealizationControl(self);
        awaken_session_contract::drive_session_realization(
            session_id,
            None,
            &control,
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

    /// Renew due local projections through the lease-only root-CAS command.
    pub async fn renew_due_session_realizations(
        &self,
        now_unix_ms: u64,
    ) -> Result<usize, SessionRealizationError> {
        const RENEW_BEFORE_MS: u64 = 150_000;
        const LEASE_MS: u64 = 300_000;
        let renew_before = now_unix_ms.saturating_add(RENEW_BEFORE_MS);
        let requested_expiry = now_unix_ms.saturating_add(LEASE_MS);
        let sessions = crate::scan_all_reconcilable_sessions(self.session_repository())
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
            let renewed_lease = self
                .renew_session_realization_after_load(RenewSessionRealization {
                    session_id: scoped.session.session_id.clone(),
                    asserted_lease: lease,
                    requested_expires_at_unix_ms: requested_expiry,
                })
                .await
                .map_err(SessionRealizationError::Control)?;
            self.runtime()
                .renew_session_realization_lease(&scoped.session.session_id, renewed_lease)
                .await
                .map_err(SessionRealizationError::Effect)?;
            renewed += 1;
        }
        Ok(renewed)
    }

    /// Recover the one local Runtime projection for every Session that lost its
    /// process incarnation or has unfinished MCP work. This is deliberately the
    /// same canonical realization driver used by create/update, not a restart-only
    /// environment or MCP path. Terminal and Worker-owned Sessions remain untouched.
    pub async fn reconcile_session_realizations(&self) -> SessionReconciliation {
        let candidates = match crate::scan_all_reconcilable_sessions(self.session_repository()).await {
            Ok(scan) => super::SessionRecoveryCandidates::from(scan),
            Err(error) => {
                let mut report = SessionReconciliation::default();
                report.failures.push(SessionReconciliationFailure {
                    session_id: "<repository>".to_string(),
                    message: error.to_string(),
                });
                return report;
            }
        };
        self.reconcile_session_realizations_from(&candidates).await
    }

    pub(super) async fn reconcile_session_realizations_from(
        &self,
        candidates: &super::SessionRecoveryCandidates,
    ) -> SessionReconciliation {
        let mut report = SessionReconciliation {
            pending: candidates.sessions.len(),
            quarantined: candidates.quarantined.clone(),
            ..Default::default()
        };
        if let Err(error) = self.refresh_executable_projections().await {
            report.failures.push(SessionReconciliationFailure {
                session_id: "<executable-projections>".to_string(),
                message: error,
            });
            return report;
        }
        for candidate in &candidates.sessions {
            let session = match self.session_repository().get(&candidate.session_id).await {
                Ok(session) => session,
                Err(awaken_session_contract::SessionRepositoryError::NotFound) => continue,
                Err(error) => {
                    report.failures.push(SessionReconciliationFailure {
                        session_id: candidate.session_id.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
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
            if !environment_admits_realization_effects(&session.environment) {
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
            match self.realize_session_after_refresh(&session_id).await {
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

    async fn realization_directive(
        &self,
        owner_scope: String,
        session: &PersistedSession,
        action: SessionRealizationAction,
        activate_pending_resources: bool,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        // Context is a lightweight rebuildable projection and must refresh even
        // when an existing realization lease needs no Environment/Resource
        // Stage. Otherwise a durable System command accepted between Runs would
        // remain invisible on a warm local or remote Worker.
        let materialize_request_context = true;
        let projection = if activate_pending_resources {
            self.frozen_session_projection(owner_scope, session, materialize_request_context)
                .await
        } else {
            self.active_frozen_session_projection(owner_scope, session, materialize_request_context)
                .await
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

    async fn next_action(
        &self,
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
            return self
                .realization_directive(
                    owner_scope,
                    session,
                    SessionRealizationAction::Stage {
                        prepare_session,
                        mcp_stages: stages,
                    },
                    activate_pending_resources,
                )
                .await;
        }
        let publish = Self::publication_generations(session)?;
        let drain = Self::draining_generations(session)?;
        let action = if publish.is_empty() && drain.is_empty() {
            SessionRealizationAction::Complete
        } else {
            SessionRealizationAction::Publish { publish, drain }
        };
        self.realization_directive(owner_scope, session, action, activate_pending_resources)
            .await
    }

    async fn session_for_realization_without_credential_migration(
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
            return Err(SessionRealizationControlFailure::Terminal);
        }
        if session.frozen_baseline().is_none() {
            return Err(SessionRealizationControlFailure::NotReady);
        }
        if !environment_admits_realization_effects(&session.environment) {
            return Err(SessionRealizationControlFailure::NotReady);
        }
        Ok((owner_scope, session))
    }

    async fn session_for_realization(
        &self,
        session_id: &str,
    ) -> Result<(String, PersistedSession), SessionRealizationControlFailure> {
        let (owner_scope, session) = self
            .session_for_realization_without_credential_migration(session_id)
            .await?;
        let session = self
            .ensure_repository_credentials_pinned(&owner_scope, session)
            .await
            .map_err(unavailable)?;
        Ok((owner_scope, session))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionRecoveryCycle {
    pub(crate) retryable_failures: usize,
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

impl SessionApplication {

    async fn claim_next_terminal_cleanup_after_refresh(
        &self,
        target: awaken_session_contract::SessionRealizationTarget,
    ) -> Result<Option<SessionTerminalCleanupAssignment>, SessionRealizationControlFailure> {
        validate_realization_target(&target)?;
        if target.reassign_existing_lease {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal cleanup recovery claims cannot reassign a live logical owner".into(),
            ));
        }
        let mut conflicted_session_id = None;
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let scan = crate::scan_all_reconcilable_sessions(self.session_repository())
                .await
                .map_err(repository_control)?;
            let mut first_projection_failure = None;
            let mut retry_after_conflict = false;

            for scoped in scan.sessions {
                let owner_scope = scoped.workspace_id;
                let mut session = scoped.session;
                if !self.requires_external_realization(&session)
                    || !session.is_terminal()
                    || !session.terminal_cleanup.is_requested()
                {
                    continue;
                }
                let pending_commands = match session
                    .terminal_cleanup
                    .pending_commands(&session.session_id)
                {
                    Ok(commands) => commands,
                    Err(error) => {
                        first_projection_failure.get_or_insert_with(|| {
                            SessionRealizationControlFailure::Invalid(error.to_string())
                        });
                        continue;
                    }
                };
                let publication_pending = match session
                    .terminal_cleanup
                    .publication_command(&session.session_id)
                {
                    Ok(command) => command.is_some(),
                    Err(error) => {
                        first_projection_failure.get_or_insert_with(|| {
                            SessionRealizationControlFailure::Invalid(error.to_string())
                        });
                        continue;
                    }
                };
                if pending_commands.is_empty() && !publication_pending {
                    continue;
                }

                // A storage adapter may lose the successful CAS response. Only
                // this invocation's exact attempted Session may replay the now
                // current assignment; ordinary later claim-next calls skip it
                // so a faulted cleanup cannot starve the remaining scan.
                let replay_after_conflict = conflicted_session_id.as_deref()
                    == Some(session.session_id.as_str())
                    && session.realization.as_ref().is_some_and(|current| {
                        current.owner == target.owner
                            && current.runtime_incarnation == target.runtime_incarnation
                            && current.expires_at_unix_ms >= target.lease_expires_at_unix_ms
                    });
                if replay_after_conflict {
                    let lease = session
                        .realization
                        .clone()
                        .expect("an exact conflict replay lease was checked");
                    match self
                        .active_frozen_session_projection(owner_scope, &session, false)
                        .await
                    {
                        Ok(projection) => {
                            return Ok(Some(SessionTerminalCleanupAssignment {
                                session_id: session.session_id,
                                projection,
                                lease,
                            }));
                        }
                        Err(error) => {
                            first_projection_failure.get_or_insert_with(|| unavailable(error));
                            continue;
                        }
                    }
                }
                if !terminal_cleanup_claim_needs_assignment(
                    session.realization.as_ref(),
                    &target,
                    now_unix_ms(),
                ) {
                    continue;
                }

                let epoch = match session.realization.as_ref() {
                    Some(current) => match current.epoch.checked_add(1) {
                        Some(epoch) => epoch,
                        None => {
                            first_projection_failure.get_or_insert_with(|| {
                                SessionRealizationControlFailure::Invalid(
                                    "Session realization lease epoch is exhausted".into(),
                                )
                            });
                            continue;
                        }
                    },
                    None => 1,
                };
                let lease = SessionRealizationLease {
                    owner: target.owner.clone(),
                    runtime_incarnation: target.runtime_incarnation.clone(),
                    epoch,
                    expires_at_unix_ms: target.lease_expires_at_unix_ms,
                };
                let session_id = session.session_id.clone();
                session.realization = Some(lease.clone());
                let committed = match self
                    .commit_session_snapshot(
                        &owner_scope,
                        session,
                        "claim-terminal-cleanup-recovery",
                        Vec::new(),
                    )
                    .await
                {
                    Ok(committed) => committed,
                    Err(SessionMutationError::Conflict)
                        if attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                    {
                        conflicted_session_id = Some(session_id);
                        retry_after_conflict = true;
                        break;
                    }
                    Err(SessionMutationError::Conflict) => {
                        return Err(SessionRealizationControlFailure::Conflict);
                    }
                    Err(error) => return Err(unavailable(error)),
                };
                match self
                    .active_frozen_session_projection(owner_scope, &committed, false)
                    .await
                {
                    Ok(projection) => {
                        return Ok(Some(SessionTerminalCleanupAssignment {
                            session_id,
                            projection,
                            lease,
                        }));
                    }
                    Err(error) => {
                        // The root claim is durable. Skip this exact incarnation
                        // on the remainder of this scan so an unavailable frozen
                        // dependency cannot head-of-line block another cleanup;
                        // expiry makes the failed assignment claimable again.
                        first_projection_failure.get_or_insert_with(|| unavailable(error));
                    }
                }
            }
            if retry_after_conflict {
                continue;
            }
            return match first_projection_failure {
                Some(error) => Err(error),
                None => Ok(None),
            };
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    async fn terminal_cleanup_commands_after_refresh(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<Vec<awaken_session_contract::SessionCleanupCommand>>,
        SessionRealizationControlFailure,
    > {
        self.external_terminal_cleanup_commands(session_id, lease)
            .await
    }

    async fn record_terminal_cleanup_completion_after_refresh(
        &self,
        lease: &SessionRealizationLease,
        completion: awaken_session_contract::SessionCleanupCompletion,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_external_terminal_cleanup_completion(lease, completion)
            .await
    }
}

/// Local application drivers cross an executable-refresh boundary before they
/// enter the multi-phase protocol. This adapter reuses that proof for the
/// Stage/Activate/Acknowledge calls without adding a token, cache, or cursor.
struct RefreshedSessionRealizationControl<'a>(&'a SessionApplication);

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
