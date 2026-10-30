// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Stateless lease companion for the canonical cleanup drive. The
// @formal reason: application owns renewal CAS; this executor polls effects only
// @formal reason: while an exact-generation renewal receipt authorizes them.
//! Keep a cleanup drive under its exact, renewed lease without owning durable state.

use crate::SessionTerminalCleanupDriveOutcome;
use awaken_session_realization_contract::{
    RenewSessionRealization, SessionRealizationControlFailure, SessionRealizationDriveError,
    SessionTerminalCleanupAssignment,
};
use std::future::Future;

/// Shared default horizon for co-located Session realization and cleanup.
pub const LOCAL_SESSION_REALIZATION_LEASE_MS: u64 = 300_000;
/// Renew while two thirds of the selected horizon remain.
pub const LOCAL_TERMINAL_CLEANUP_RENEW_INTERVAL_MS: u64 = LOCAL_SESSION_REALIZATION_LEASE_MS / 3;

/// Drive and renew the original frozen cleanup assignment. A failed renewal
/// stops polling the drive; only exact-generation authority permits it to resume.
pub async fn supervise_session_terminal_cleanup_lease<Drive, Renew, RenewFuture, Clock>(
    session_id: &awaken_session_types::SessionId,
    initial_assignment: &SessionTerminalCleanupAssignment,
    realization: &dyn awaken_session_execution_contract::RealizationRuntime,
    runtime: &dyn awaken_session_execution_contract::TerminalCleanup,
    drive: Drive,
    mut renew: Renew,
    now_unix_ms: Clock,
) -> Result<SessionTerminalCleanupDriveOutcome, SessionRealizationDriveError>
where
    Drive:
        Future<Output = Result<SessionTerminalCleanupDriveOutcome, SessionRealizationDriveError>>,
    Renew: FnMut(RenewSessionRealization) -> RenewFuture,
    RenewFuture: Future<
        Output = Result<
            awaken_session_realization_contract::SessionRealizationLeaseReceipt,
            SessionRealizationControlFailure,
        >,
    >,
    Clock: Fn() -> u64,
{
    if initial_assignment.session_id != session_id {
        return Err(SessionRealizationDriveError::Control(
            SessionRealizationControlFailure::Invalid(
                "local terminal cleanup assignment names another Session".into(),
            ),
        ));
    }
    let mut asserted_lease = initial_assignment.authority.lease.clone();
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(
        LOCAL_TERMINAL_CLEANUP_RENEW_INTERVAL_MS,
    ));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Tokio intervals have one immediately-ready first tick. Consume it before
    // entering the biased race so a short cleanup never writes a needless root
    // renewal merely because its drive and the synthetic first tick are ready
    // together.
    interval.tick().await;
    let mut drive = std::pin::pin!(drive);

    loop {
        tokio::select! {
            biased;
            outcome = &mut drive => return outcome,
            _ = interval.tick() => {
                // This companion owns no effect or retry state. While renewal
                // is in flight the canonical drive is deliberately not polled;
                // only a verified same-generation CAS readback lets it resume.
                let requested_expiry_unix_ms = now_unix_ms()
                    .checked_add(LOCAL_SESSION_REALIZATION_LEASE_MS)
                    .map(|requested| requested.max(asserted_lease.expires_at_unix_ms))
                    .ok_or_else(|| {
                        SessionRealizationDriveError::Control(
                            SessionRealizationControlFailure::Unavailable(
                                "local terminal cleanup renewal expiry overflow".into(),
                            ),
                        )
                    })?;
                let command = RenewSessionRealization {
                    session_id: session_id.clone(),
                    asserted_lease: asserted_lease.clone(),
                    requested_expires_at_unix_ms: requested_expiry_unix_ms,
                };
                let request_started = realization.observe_realization_request()?;
                let receipt = renew(command)
                    .await
                    .map_err(SessionRealizationDriveError::Control)?;
                let observed = receipt.clone().observed(request_started);
                if observed.remaining_at(realization.observe_realization_request()?).is_none() {
                    return Err(SessionRealizationDriveError::Control(SessionRealizationControlFailure::StaleOwnership));
                }
                let renewed_lease = observed.lease.clone();
                if !awaken_session_realization_contract::realization_lease_generation_authorizes(
                        &renewed_lease,
                        &asserted_lease,
                    )
                    || renewed_lease.expires_at_unix_ms < requested_expiry_unix_ms
                {
                    return Err(SessionRealizationDriveError::Control(
                        SessionRealizationControlFailure::Invalid(
                            "local terminal cleanup renewal returned another or regressed generation"
                                .into(),
                        ),
                    ));
                }
                // Renewal is deliberately lease-only. Reinstall the original
                // aggregate-frozen terminal projection under that monotonic
                // successor; neither Control nor this companion rebuilds a
                // second projection or terminal-specific renewal read model.
                let renewed_assignment = SessionTerminalCleanupAssignment {
                    environment_creation_not_attempted: initial_assignment.environment_creation_not_attempted,
                    session_id: initial_assignment.session_id.clone(),
                    projection: initial_assignment.projection.clone(),
                    authority: receipt,
                };
                runtime.install_terminal_cleanup_assignment(&renewed_assignment, observed)
                    .await
                    .map_err(SessionRealizationDriveError::from)?;
                asserted_lease = renewed_lease;
            }
        }
    }
}
