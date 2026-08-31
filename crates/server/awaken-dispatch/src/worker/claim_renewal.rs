//! One exact dispatch claim's renewal and cancellation-signal lifecycle.
//!
//! Durable lease truth stays in [`Dispatch`]. This module owns only the one
//! process-local guard that keeps a current claim live and projects loss of
//! provable ownership into Runtime's existing cooperative cancellation token.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::clock::Clock;
use crate::dispatch::{Dispatch, RunClaim};

/// One exact claim's renewal lifecycle. Renewal belongs beside the drive that
/// owns the claim, rather than to each caller (pool, daemon, or foreground child),
/// so every execution path has the same lease behavior.
pub(crate) struct ClaimLeaseRenewal {
    shutdown: CancellationToken,
    attempt_cancellation: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ClaimLeaseRenewal {
    fn drop(&mut self) {
        self.attempt_cancellation.cancel();
        self.shutdown.cancel();
        self.task.abort();
    }
}

impl ClaimLeaseRenewal {
    pub(crate) fn attempt_cancellation(&self) -> CancellationToken {
        self.attempt_cancellation.clone()
    }
}

/// Owns the one signal-only bridge needed when a claimed drive also has a host
/// cancellation token. Runtime still consumes one canonical cooperative token;
/// this relay merely makes either existing authority observable through it.
pub(crate) struct AttemptCancellationRelay {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for AttemptCancellationRelay {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(crate) fn combine_attempt_cancellation(
    claim_cancellation: CancellationToken,
    host_cancellation: Option<&CancellationToken>,
) -> (CancellationToken, AttemptCancellationRelay) {
    let Some(host_cancellation) = host_cancellation.cloned() else {
        return (claim_cancellation, AttemptCancellationRelay { task: None });
    };
    let combined = CancellationToken::new();
    if claim_cancellation.is_cancelled() || host_cancellation.is_cancelled() {
        combined.cancel();
        return (combined, AttemptCancellationRelay { task: None });
    }
    let relay_target = combined.clone();
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = claim_cancellation.cancelled() => {}
            _ = host_cancellation.cancelled() => {}
        }
        relay_target.cancel();
    });
    (combined, AttemptCancellationRelay { task: Some(task) })
}

/// Keep an exact claim live across any owned work, including the potentially
/// slow Worker/Session resolution that precedes the Runtime drive.
pub(crate) fn renew_claim_while_active<S: Dispatch + 'static>(
    store: Arc<S>,
    claim: &RunClaim,
    lease_ms: u64,
    clock: Arc<dyn Clock>,
) -> ClaimLeaseRenewal {
    let claim = claim.clone();
    let interval_ms = (lease_ms / 3).max(1);
    let interval = Duration::from_millis(interval_ms);
    let request_timeout = Duration::from_millis((interval_ms / 2).max(1));
    let retry_delay = Duration::from_millis((interval_ms / 10).clamp(1, 1_000));
    // Stop locally before another node may legitimately recover the lease. This
    // is a conservative proof window, not a second durable expiry authority.
    let proof_window = Duration::from_millis(lease_ms.saturating_sub(interval_ms).max(1));
    let shutdown = CancellationToken::new();
    let attempt_cancellation = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task_attempt_cancellation = attempt_cancellation.clone();
    let task = tokio::spawn(async move {
        let started = Instant::now();
        let mut next_regular_renewal = started + interval;
        let mut next_attempt = next_regular_renewal;
        let mut proof_deadline = started + proof_window;
        loop {
            tokio::select! {
                biased;
                _ = task_shutdown.cancelled() => break,
                _ = tokio::time::sleep_until(proof_deadline) => {
                    tracing::warn!(
                        run_id = %claim.run_id.0,
                        owner = %claim.owner,
                        lease_epoch = claim.epoch,
                        "dispatch lease renewal could not prove ownership before safety deadline"
                    );
                    task_attempt_cancellation.cancel();
                    break;
                }
                _ = tokio::time::sleep_until(next_attempt) => {
                    let attempt_started = Instant::now();
                    let attempt_timeout = request_timeout.min(
                        proof_deadline.saturating_duration_since(attempt_started),
                    );
                    while next_regular_renewal <= attempt_started {
                        next_regular_renewal += interval;
                    }
                    let renewal = tokio::time::timeout(
                        attempt_timeout,
                        store.renew_lease(&claim, lease_ms, clock.now_ms()),
                    )
                    .await;
                    match renewal {
                        Ok(Ok(true)) => {
                            proof_deadline = Instant::now() + proof_window;
                            next_attempt = next_regular_renewal;
                        }
                        Ok(Ok(false)) => {
                            tracing::warn!(
                                run_id = %claim.run_id.0,
                                owner = %claim.owner,
                                lease_epoch = claim.epoch,
                                "dispatch lease renewal lost exact claim ownership"
                            );
                            // Exact ownership is already authoritatively lost.
                            // Wake Runtime's existing cooperative cancellation
                            // seam now; waiting for the provider call to return
                            // only creates stale checkpoints and wasted usage.
                            task_attempt_cancellation.cancel();
                            break;
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(
                                run_id = %claim.run_id.0,
                                owner = %claim.owner,
                                lease_epoch = claim.epoch,
                                %error,
                                "dispatch lease renewal failed; retrying within safety window"
                            );
                            next_attempt = (Instant::now() + retry_delay).min(proof_deadline);
                        }
                        Err(_) => {
                            tracing::warn!(
                                run_id = %claim.run_id.0,
                                owner = %claim.owner,
                                lease_epoch = claim.epoch,
                                timeout_ms = attempt_timeout.as_millis(),
                                "dispatch lease renewal timed out; retrying within safety window"
                            );
                            next_attempt = (Instant::now() + retry_delay).min(proof_deadline);
                        }
                    }
                }
            }
        }
    });
    ClaimLeaseRenewal {
        shutdown,
        attempt_cancellation,
        task,
    }
}
