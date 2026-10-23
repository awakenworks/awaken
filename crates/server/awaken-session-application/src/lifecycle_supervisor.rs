// @formal entrypoint session_realization.recovery_backoff_is_bounded_and_dependency_wakes_never_postpone_the_existing_deadline
//! The one process-owned Session lifecycle task and its recovery cadence.
//!
//! Construction, cancellation, maintenance and task joining retain the same
//! ordering as the original realization module. The task carries only its
//! diagnostics across cycles; all durable work still comes from the root scan.

use super::SessionApplication;
use super::realization::recovery_backoff::{
    SessionRecoveryTrigger, external_wake_coalescing_delay, next_external_wake_cooldown,
    next_session_recovery_delay,
};

async fn run_lifecycle_maintenance(application: &SessionApplication) {
    if let Err(error) = application
        .realizations
        .renew_due_session_realizations()
        .await
    {
        tracing::warn!(
            error = ?error,
            "Session realization lease renewal remains pending"
        );
    }
    reconcile_dispatch(application).await;
}

async fn reconcile_dispatch(application: &SessionApplication) {
    let dispatch = application.work_assignments.reconcile().await;
    for failure in dispatch.failures {
        tracing::warn!(
            session = %failure.session_id,
            environment = %failure.environment_id,
            error = %failure.message,
            "Session WorkQueue dispatch remains pending"
        );
    }
}

pub(super) async fn run_lifecycle_loop(
    application: std::sync::Arc<SessionApplication>,
    cancellation: awaken_runtime_contract::CancellationToken,
) -> Result<(), String> {
    let mut recovery_failure_streak = 0_u32;
    let mut continuation_diagnostics = super::recovery_scan::ContinuationDiagnostics::default();
    let mut next_recovery = awaken_time::mono::now();
    let mut external_wake_cooldown = awaken_time::mono::now();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    reconcile_dispatch(&application).await;
    loop {
        let recovery_trigger = tokio::select! {
            () = cancellation.cancelled() => break,
            _ = application.lifecycle.notified() => {
                let delay = external_wake_coalescing_delay(
                    next_recovery.saturating_duration_since(awaken_time::mono::now()),
                    external_wake_cooldown
                        .saturating_duration_since(awaken_time::mono::now()),
                );
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => break,
                    () = tokio::time::sleep(delay) => {}
                }
                Some(SessionRecoveryTrigger::ExternalWake)
            },
            _ = tokio::time::sleep_until(next_recovery) => Some(SessionRecoveryTrigger::Scheduled),
            _ = interval.tick() => {
                run_lifecycle_maintenance(&application).await;
                None
            }
        };
        let Some(recovery_trigger) = recovery_trigger else {
            continue;
        };
        // The sole recovery cycle composes Resource, realization,
        // Event-batch, and Outcome reconciliation. Await it as a distinct
        // Tokio task so its first poll begins at the scheduler boundary;
        // nesting the complete state machine in this supervisor can exceed
        // a default worker stack before the first User batch is admitted.
        let recovery_application = std::sync::Arc::clone(&application);
        let mut recovery_task = tokio::task::JoinSet::new();
        recovery_task.spawn(async move {
            let cycle = recovery_application
                .reconcile_pending_session_state_for(
                    recovery_trigger,
                    &mut continuation_diagnostics,
                )
                .await;
            (cycle, continuation_diagnostics)
        });
        let (cycle, returned_diagnostics) = loop {
            tokio::select! {
                biased;
                result = recovery_task.join_next() => {
                    break result
                        .ok_or_else(|| "Session recovery task disappeared".to_string())?
                        .map_err(|error| format!("Session recovery task failed: {error}"))?;
                }
                _ = interval.tick() => run_lifecycle_maintenance(&application).await,
            }
        };
        continuation_diagnostics = returned_diagnostics;
        recovery_failure_streak = if cycle.retryable_failures == 0 {
            0
        } else {
            recovery_failure_streak.saturating_add(1)
        };
        let now = awaken_time::mono::now();
        external_wake_cooldown =
            now + next_external_wake_cooldown(recovery_trigger, cycle.settled, cycle.elapsed);
        let existing_remaining = next_recovery.saturating_duration_since(now);
        next_recovery = now
            + next_session_recovery_delay(
                recovery_trigger,
                existing_remaining,
                recovery_failure_streak,
            );
    }
    Ok(())
}
/// Existing event polling boundary, retaining its structurally owned task.
/// T1 child success -> exact report; T2 panic -> original task failure;
/// T3 missing child -> original disappeared report; T4 caller cancellation
/// -> JoinSet drops/aborts child. No failure invents a durable settlement.
pub(super) async fn reconcile_event_batch_recovery_task(
    application: &std::sync::Arc<SessionApplication>,
    candidates: &super::SessionRecoveryCandidates,
) -> crate::event_batches::EventBatchReconciliation {
    // Event reconciliation may enter the complete Runtime Run state machine.
    // Start that phase at a scheduler boundary instead of nesting it below
    // Resource, continuation, and realization recovery. The `JoinSet` keeps
    // the child structurally owned and aborts it if this recovery cycle is
    // cancelled; only its polling stack changes.
    let application = std::sync::Arc::clone(application);
    let mut event_task = tokio::task::JoinSet::new();
    let event_candidates = candidates.clone();
    event_task.spawn(async move {
        application
            .reconcile_event_batches_from(&event_candidates)
            .await
    });
    let message = match event_task.join_next().await {
        Some(Ok(report)) => return report,
        Some(Err(error)) => format!("Session Event recovery task failed: {error}"),
        None => "Session Event recovery task disappeared".to_string(),
    };
    let mut report = crate::event_batches::EventBatchReconciliation::default();
    report.failures.push(("<supervisor>".to_string(), message));
    report
}
