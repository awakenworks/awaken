// @formal entrypoint run_ingress.renewal_projects_cancel_only_to_exact_physical_attempt
//! One exact dispatch claim's renewal and cancellation-signal lifecycle.
//!
//! Durable lease truth stays in [`Dispatch`]. This module owns only the one
//! process-local guard that keeps a current claim live and projects loss of
//! provable ownership into Runtime's existing cooperative cancellation token,
//! tagged with the cause that decides whether the Run may end `Cancelled`.

use awaken_dispatch_erasure::Dispatch;
use std::sync::Arc;

use awaken_lease_contract::authority::AuthorityLeaseTiming;
use awaken_runtime_contract::cancellation::{AttemptCancellation, CancellationCause};
use awaken_time::mono;
use tokio_util::sync::CancellationToken;

use crate::dispatch::{LeaseRenewalOutcome, RunClaim};

/// Why one claim's renewal loop fired the attempt's cancellation.
///
/// | Row | Renewal observation | Recorded cause | Run effect |
/// |---|---|---|---|
/// | RC1 | durable cancellation of this exact attempt | `Requested` | commit `Cancelled` |
/// | RC2 | authoritative loss of the exact claim | `ClaimLost` | abandon, no commit |
/// | RC3 | no renewal before the local proof window closed | `ProofExpired` | abandon, no commit |
/// | RC4 | the owning drive dropped its renewal guard | `HostDrain` | abandon, no commit |
///
/// Only RC1 is the Run's cancellation authority. RC2–RC4 stop this process's
/// attempt; the claim lapses and Dispatch recovery re-claims the Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RenewalStop {
    DurableCancellation,
    ClaimLost,
    ProofExpired,
    OwnerDropped,
}

impl RenewalStop {
    pub(crate) const fn cause(self) -> CancellationCause {
        match self {
            Self::DurableCancellation => CancellationCause::Requested,
            Self::ClaimLost => CancellationCause::ClaimLost,
            Self::ProofExpired => CancellationCause::ProofExpired,
            Self::OwnerDropped => CancellationCause::HostDrain,
        }
    }
}

/// One exact claim's renewal lifecycle. Renewal belongs beside the drive that
/// owns the claim, rather than to each caller (pool, daemon, or foreground child),
/// so every execution path has the same lease behavior.
pub(crate) struct ClaimLeaseRenewal {
    shutdown: CancellationToken,
    attempt_cancellation: AttemptCancellation,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ClaimLeaseRenewal {
    fn drop(&mut self) {
        self.attempt_cancellation
            .cancel(RenewalStop::OwnerDropped.cause());
        self.shutdown.cancel();
        self.task.abort();
    }
}

impl ClaimLeaseRenewal {
    pub(crate) fn attempt_cancellation(&self) -> AttemptCancellation {
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

fn relay_host_signal(host: &AttemptCancellation, target: &AttemptCancellation) {
    match host.cause.recorded() {
        Some(cause) => target.cancel(cause),
        None => target.token.cancel(),
    }
}

/// The combined signal shares the claim's cause cell, so a claim-side cause is
/// already recorded when the relay fires. A host-side signal carries the
/// cause its own source recorded; an untagged host signal stays untagged
/// (a direct request), exactly as before causes existed.
pub(crate) fn combine_attempt_cancellation(
    claim_cancellation: AttemptCancellation,
    host_cancellation: Option<&AttemptCancellation>,
) -> (AttemptCancellation, AttemptCancellationRelay) {
    let Some(host_cancellation) = host_cancellation.cloned() else {
        return (claim_cancellation, AttemptCancellationRelay { task: None });
    };
    let combined = AttemptCancellation {
        token: CancellationToken::new(),
        cause: claim_cancellation.cause.clone(),
    };
    if claim_cancellation.token.is_cancelled() {
        combined.token.cancel();
        return (combined, AttemptCancellationRelay { task: None });
    }
    if host_cancellation.token.is_cancelled() {
        relay_host_signal(&host_cancellation, &combined);
        return (combined, AttemptCancellationRelay { task: None });
    }
    let relay_target = combined.clone();
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = claim_cancellation.token.cancelled() => relay_target.token.cancel(),
            _ = host_cancellation.token.cancelled() => {
                relay_host_signal(&host_cancellation, &relay_target);
            }
        }
    });
    (combined, AttemptCancellationRelay { task: Some(task) })
}

/// Keep an exact claim live across any owned work, including the potentially
/// slow Worker/Session resolution that precedes the Runtime drive.
pub(crate) fn renew_claim_while_active<S: Dispatch + 'static>(
    store: Arc<S>,
    claim: &RunClaim,
    lease_ms: u64,
) -> ClaimLeaseRenewal {
    let claim = claim.clone();
    let timing = AuthorityLeaseTiming::from_ttl_ms(lease_ms);
    let interval = timing.renew_interval();
    let retry_delay = timing.retry_delay();
    // Stop locally before another node may legitimately recover the lease. This
    // is a conservative proof window, not a second durable expiry authority.
    let proof_window = timing.proof_window();
    let shutdown = CancellationToken::new();
    let attempt_cancellation = AttemptCancellation::default();
    let task_shutdown = shutdown.clone();
    let task_attempt_cancellation = attempt_cancellation.clone();
    let task = tokio::spawn(async move {
        let started = mono::now();
        let mut next_regular_renewal = started + interval;
        let mut next_attempt = next_regular_renewal;
        let mut proof_deadline = started + proof_window;
        let mut consecutive_failures = 0_u64;
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
                    task_attempt_cancellation.cancel(RenewalStop::ProofExpired.cause());
                    break;
                }
                _ = tokio::time::sleep_until(next_attempt) => {
                    let attempt_started = mono::now();
                    // A renewal may run for the whole remaining proof: a slow
                    // reply inside it still renews the claim. An exhausted
                    // proof starts no request; the deadline arm above ends it.
                    let Some(attempt_timeout) = timing.renewal_request_budget(
                        proof_deadline.saturating_duration_since(attempt_started),
                    ) else {
                        continue;
                    };
                    while next_regular_renewal <= attempt_started {
                        next_regular_renewal += interval;
                    }
                    let renewal = tokio::time::timeout(
                        attempt_timeout,
                        store.renew_lease(&claim, lease_ms),
                    )
                    .await;
                    match renewal {
                        Ok(Ok(LeaseRenewalOutcome::Current)) => {
                            consecutive_failures = 0;
                            // Renewal was applied sometime after dispatch and no
                            // later than receipt. Anchor at dispatch so transport
                            // latency cannot extend the durable lease locally.
                            let received_at = mono::now();
                            proof_deadline = received_at
                                + timing.remaining_proof_after(
                                    received_at.saturating_duration_since(attempt_started),
                                );
                            next_attempt = next_regular_renewal;
                        }
                        Ok(Ok(LeaseRenewalOutcome::CancellationRequested)) => {
                            tracing::info!(
                                run_id = %claim.run_id.0,
                                owner = %claim.owner,
                                lease_epoch = claim.epoch,
                                "durable cancellation requested for exact physical attempt"
                            );
                            // The Dispatch row remains the sole authority. This
                            // projects its exact cancelled-attempt observation
                            // into Runtime's already-installed cooperative token.
                            task_attempt_cancellation
                                .cancel(RenewalStop::DurableCancellation.cause());
                            break;
                        }
                        Ok(Ok(LeaseRenewalOutcome::Lost)) => {
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
                            // The Run was not cancelled: the attempt abandons.
                            task_attempt_cancellation.cancel(RenewalStop::ClaimLost.cause());
                            break;
                        }
                        Ok(Err(error)) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            let now = mono::now();
                            tracing::warn!(
                                run_id = %claim.run_id.0,
                                owner = %claim.owner,
                                lease_epoch = claim.epoch,
                                consecutive_failures,
                                proof_remaining_ms = proof_deadline.saturating_duration_since(now).as_millis(),
                                attempt_duration_ms = now.duration_since(attempt_started).as_millis(),
                                %error,
                                "dispatch lease renewal failed; retrying within safety window"
                            );
                            next_attempt = (mono::now() + retry_delay).min(proof_deadline);
                        }
                        Err(_) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            let now = mono::now();
                            tracing::warn!(
                                run_id = %claim.run_id.0,
                                owner = %claim.owner,
                                lease_epoch = claim.epoch,
                                consecutive_failures,
                                proof_remaining_ms = proof_deadline.saturating_duration_since(now).as_millis(),
                                attempt_duration_ms = now.duration_since(attempt_started).as_millis(),
                                timeout_ms = attempt_timeout.as_millis(),
                                "dispatch lease renewal timed out; retrying within safety window"
                            );
                            next_attempt = (mono::now() + retry_delay).min(proof_deadline);
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
