use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex as SyncMutex;
use tokio::sync::RwLock;

use awaken_contract::contract::identity::RunOrigin;
use awaken_contract::contract::mailbox::{
    LiveRunTarget, RunDispatch, RunDispatchResult, RunDispatchStatus,
};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::RunRecord;
use awaken_contract::contract::tool_intercept::RunMode;
use awaken_contract::now_ms;
use awaken_runtime::RunActivation;

use super::{
    DISPATCH_SIGNAL_BATCH_DEFAULT, DISPATCH_SIGNAL_BATCH_ENV,
    DISPATCH_SIGNAL_BLOCKED_NACK_BASE_DELAY_DEFAULT,
    DISPATCH_SIGNAL_BLOCKED_NACK_MAX_DELAY_DEFAULT, DISPATCH_SIGNAL_EXPIRES_DEFAULT,
    DISPATCH_SIGNAL_EXPIRES_ENV, DISPATCH_SIGNAL_MAX_CONCURRENT_HANDLERS_DEFAULT,
    DISPATCH_SIGNAL_MAX_CONCURRENT_HANDLERS_ENV, DISPATCH_SIGNAL_NACK_BASE_DELAY_ENV,
    DISPATCH_SIGNAL_NACK_MAX_DELAY_ENV, MailboxError, MailboxRunOutcome, MailboxWorker,
    MailboxWorkerStatus,
};

/// Revert worker from Claiming → Idle, but only if still in Claiming state.
/// Prevents overwriting a Running state set by a concurrent dispatch.
pub(super) async fn revert_claiming_to_idle(
    workers: &RwLock<HashMap<String, Arc<SyncMutex<MailboxWorker>>>>,
    thread_id: &str,
) {
    let workers = workers.read().await;
    if let Some(worker) = workers.get(thread_id) {
        let mut w = worker.lock();
        if matches!(w.status, MailboxWorkerStatus::Claiming) {
            w.status = MailboxWorkerStatus::Idle;
        }
    }
}

// ── Free functions ───────────────────────────────────────────────────

pub(super) fn normalize_mailbox_run_mode(request: &mut RunActivation, background: bool) {
    if request.trace.run_mode != RunMode::Foreground {
        return;
    }

    request.trace.run_mode =
        if !request.control.seeded_decisions.is_empty() || request.resume_run_id().is_some() {
            RunMode::Resume
        } else if matches!(request.trace.origin, RunOrigin::Internal) {
            RunMode::InternalWake
        } else if background {
            RunMode::Scheduled
        } else {
            RunMode::Foreground
        };
}

/// Validate and normalize run request inputs.
///
/// Checks that messages are non-empty, trims/generates thread_id.
/// Returns `(thread_id, messages)`.
/// Internal validation for mailbox submit paths.
pub(super) fn validate_run_inputs(
    thread_id: String,
    messages: Vec<Message>,
    allow_empty_messages: bool,
) -> Result<(String, Vec<Message>), MailboxError> {
    if messages.is_empty() && !allow_empty_messages {
        return Err(MailboxError::Validation(
            "at least one message is required".to_string(),
        ));
    }
    let thread_id = {
        let trimmed = thread_id.trim().to_string();
        if trimmed.is_empty() {
            uuid::Uuid::now_v7().to_string()
        } else {
            trimmed
        }
    };
    Ok((thread_id, messages))
}

pub(super) fn normalize_message_ids(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .cloned()
        .map(|mut message| {
            if message.id.as_deref().map(str::is_empty).unwrap_or(true) {
                message.id = Some(awaken_contract::contract::message::gen_message_id());
            }
            message
        })
        .collect()
}

pub(super) fn live_target_for_dispatch(dispatch: &RunDispatch) -> LiveRunTarget {
    LiveRunTarget::new(dispatch.thread_id.clone(), dispatch.run_id.clone())
        .with_dispatch_id(dispatch.dispatch_id.clone())
}

pub(super) fn live_target_for_run(run: &RunRecord) -> LiveRunTarget {
    let mut target = LiveRunTarget::new(run.thread_id.clone(), run.run_id.clone());
    if let Some(dispatch_id) = run.dispatch_id.clone() {
        target = target.with_dispatch_id(dispatch_id);
    }
    target
}

pub(super) fn mailbox_run_result(
    run_id: &str,
    dispatch_instance_id: &str,
    result: &Result<
        awaken_runtime::loop_runner::AgentRunResult,
        awaken_runtime::loop_runner::AgentLoopError,
    >,
) -> RunDispatchResult {
    use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};

    match result {
        Ok(run) => {
            let (status, _) = run.termination.to_run_status();
            RunDispatchResult {
                run_id: run.run_id.clone(),
                dispatch_instance_id: dispatch_instance_id.to_string(),
                status,
                termination: Some(run.termination.clone()),
                response: (!run.response.is_empty()).then(|| run.response.clone()),
                error: match &run.termination {
                    TerminationReason::Error(message) => Some(message.clone()),
                    _ => None,
                },
            }
        }
        Err(error) => RunDispatchResult {
            run_id: run_id.to_string(),
            dispatch_instance_id: dispatch_instance_id.to_string(),
            status: RunStatus::Done,
            termination: Some(TerminationReason::Error(error.to_string())),
            response: None,
            error: Some(error.to_string()),
        },
    }
}

pub(super) fn mailbox_run_identity(
    dispatch: &RunDispatch,
    run_id: &str,
    dispatch_instance_id: &str,
) -> awaken_contract::contract::identity::RunIdentity {
    awaken_contract::contract::identity::RunIdentity::new(
        dispatch.thread_id.clone(),
        None,
        run_id.to_string(),
        None,
        String::new(),
        awaken_contract::contract::identity::RunOrigin::Internal,
    )
    .with_dispatch_id(dispatch.dispatch_id.clone())
    .with_session_id(dispatch_instance_id.to_string())
}

pub(super) fn millis_to_seconds(ms: u64) -> f64 {
    ms as f64 / 1_000.0
}

pub(super) fn record_mailbox_dispatch_start_metrics(dispatch: &RunDispatch, start_now: u64) {
    let enqueue_to_start_ms = start_now.saturating_sub(dispatch.created_at);
    let eligible_to_start_ms = start_now.saturating_sub(dispatch.available_at);
    let claim_to_start_ms = start_now.saturating_sub(dispatch.updated_at);

    crate::metrics::record_mailbox_dispatch_enqueue_to_start(millis_to_seconds(
        enqueue_to_start_ms,
    ));
    crate::metrics::record_mailbox_dispatch_eligible_to_start(millis_to_seconds(
        eligible_to_start_ms,
    ));
    crate::metrics::record_mailbox_dispatch_claim_to_start(millis_to_seconds(claim_to_start_ms));

    tracing::info!(
        dispatch_id = %dispatch.dispatch_id,
        run_id = %dispatch.run_id,
        thread_id = %dispatch.thread_id,
        enqueue_to_start_ms,
        eligible_to_start_ms,
        claim_to_start_ms,
        "mailbox dispatch processing started"
    );
}

pub(super) fn record_mailbox_dispatch_completion_metrics(
    dispatch: &RunDispatch,
    start_now: u64,
    completed_now: u64,
    outcome: &str,
) {
    let runtime_ms = completed_now.saturating_sub(start_now);
    let enqueue_to_complete_ms = completed_now.saturating_sub(dispatch.created_at);

    crate::metrics::record_mailbox_dispatch_runtime(millis_to_seconds(runtime_ms), outcome);
    crate::metrics::record_mailbox_dispatch_enqueue_to_complete(
        millis_to_seconds(enqueue_to_complete_ms),
        outcome,
    );
    crate::metrics::record_run_completion(millis_to_seconds(runtime_ms), outcome);

    tracing::info!(
        dispatch_id = %dispatch.dispatch_id,
        run_id = %dispatch.run_id,
        thread_id = %dispatch.thread_id,
        outcome,
        runtime_ms,
        enqueue_to_complete_ms,
        "mailbox dispatch processing completed"
    );
}

pub(super) fn record_mailbox_dispatch_terminal_metrics(dispatch: &RunDispatch, outcome: &str) {
    let completed_now = dispatch.completed_at.unwrap_or_else(now_ms);
    record_mailbox_dispatch_completion_metrics(dispatch, completed_now, completed_now, outcome);
}

pub(super) fn record_mailbox_operation_result(operation: &str, result: &str, start: Instant) {
    crate::metrics::record_mailbox_operation(operation, result, start.elapsed().as_secs_f64());
}

pub(super) fn dispatch_signal_blocked_nack_delay(redelivery_attempts: Option<u64>) -> Duration {
    let exponent = redelivery_attempts.unwrap_or(1).saturating_sub(1).min(16);
    let multiplier = 1u32.checked_shl(exponent as u32).unwrap_or(u32::MAX);
    dispatch_signal_nack_base_delay()
        .saturating_mul(multiplier)
        .min(dispatch_signal_nack_max_delay())
}

pub(super) fn dispatch_signal_batch_size() -> usize {
    env_usize(DISPATCH_SIGNAL_BATCH_ENV, DISPATCH_SIGNAL_BATCH_DEFAULT)
}

pub(super) fn dispatch_signal_fetch_expires() -> Duration {
    env_duration_ms(DISPATCH_SIGNAL_EXPIRES_ENV, DISPATCH_SIGNAL_EXPIRES_DEFAULT)
}

pub(super) fn dispatch_signal_nack_base_delay() -> Duration {
    env_duration_ms(
        DISPATCH_SIGNAL_NACK_BASE_DELAY_ENV,
        DISPATCH_SIGNAL_BLOCKED_NACK_BASE_DELAY_DEFAULT,
    )
}

pub(super) fn dispatch_signal_nack_max_delay() -> Duration {
    env_duration_ms(
        DISPATCH_SIGNAL_NACK_MAX_DELAY_ENV,
        DISPATCH_SIGNAL_BLOCKED_NACK_MAX_DELAY_DEFAULT,
    )
}

pub(super) fn dispatch_signal_max_concurrent_handlers() -> usize {
    env_usize(
        DISPATCH_SIGNAL_MAX_CONCURRENT_HANDLERS_ENV,
        DISPATCH_SIGNAL_MAX_CONCURRENT_HANDLERS_DEFAULT,
    )
}

pub(super) fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

pub(super) fn env_duration_ms(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(default)
}

pub(super) fn result_label<T, E>(result: &Result<T, E>) -> &'static str {
    if result.is_ok() { "ok" } else { "error" }
}

pub(super) fn dispatch_status_label(status: RunDispatchStatus) -> &'static str {
    match status {
        RunDispatchStatus::Queued => "queued",
        RunDispatchStatus::Claimed => "claimed",
        RunDispatchStatus::Acked => "acked",
        RunDispatchStatus::Cancelled => "cancelled",
        RunDispatchStatus::Superseded => "superseded",
        RunDispatchStatus::DeadLetter => "dead_letter",
    }
}

/// Classify a runtime run result for ack/nack/dead_letter.
pub(super) fn classify_error(
    result: &Result<
        awaken_runtime::loop_runner::AgentRunResult,
        awaken_runtime::loop_runner::AgentLoopError,
    >,
) -> MailboxRunOutcome {
    match result {
        Ok(_) => MailboxRunOutcome::Completed,
        Err(e) => {
            use awaken_runtime::loop_runner::AgentLoopError;
            match e {
                AgentLoopError::RuntimeError(re) => {
                    use awaken_runtime::RuntimeError;
                    match re {
                        RuntimeError::ThreadAlreadyRunning { .. } => {
                            // After the cancel-on-submit change, this error
                            // indicates a race that retrying won't fix.
                            MailboxRunOutcome::PermanentError(e.to_string())
                        }
                        RuntimeError::AgentNotFound { .. } | RuntimeError::ResolveFailed { .. } => {
                            MailboxRunOutcome::PermanentError(e.to_string())
                        }
                        _ => MailboxRunOutcome::TransientError(e.to_string()),
                    }
                }
                AgentLoopError::StorageError(_) => MailboxRunOutcome::TransientError(e.to_string()),
                AgentLoopError::InferenceFailed(_) => {
                    MailboxRunOutcome::TransientError(e.to_string())
                }
                // Agent-level failures (phase error, invalid resume) are not infra errors.
                _ => MailboxRunOutcome::Completed,
            }
        }
    }
}
