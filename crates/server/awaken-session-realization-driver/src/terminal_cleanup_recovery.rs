// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the one terminal cleanup recovery orchestration (absence handling,
// @formal reason: completed-projection retirement, settled-assignment revocation and the bounded claim
// @formal reason: sweep) over contract participants. It persists no cursor, retry record or phase: the Session
// @formal reason: aggregate stays the only queue and every physical effect runs through the terminal
// @formal reason: cleanup driver, so this is executable orchestration evidence, not a formal-proof claim.
//! Terminal cleanup recovery: the one orchestration that turns a claimed or
//! re-read terminal assignment into the driver's drive, a process-local
//! retirement, or a revocation.
//!
//! The Session executor owns the process-local projections and serves them
//! through [`TerminalRecoveryProjections`]; it decides nothing here. Which
//! projection is retired, when Run controls are signalled, how a settled
//! assignment is revoked and how one recovery sweep is bounded are decided
//! once, in this module.

use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_execution_contract::{RealizationEffects, RealizationRuntime, TerminalCleanup};
use awaken_session_realization_contract::{
    SessionRealizationControlFailure, SessionRealizationDriveError, SessionRealizationTarget,
    SessionTerminalCleanupControl,
};
use awaken_session_types::{RunError, SessionId};
use futures_util::StreamExt as _;

/// The most assignments one recovery sweep claims, so one sweep cannot
/// monopolize the Worker.
pub const MAX_ASSIGNMENTS_PER_RECOVERY_SWEEP: usize = 64;
/// The most Sessions one recovery sweep drives at a time.
pub const MAX_CONCURRENT_TERMINAL_RECOVERIES: usize = 4;

/// The process-local projections a terminal recovery acts on, served by the
/// executor that installs them. Each method performs one named local effect;
/// the recovery decides which one and when.
#[async_trait::async_trait]
pub trait TerminalRecoveryProjections: Send + Sync {
    /// The installed projection a terminal lookup belongs to, observed before
    /// any Control I/O: its realization phase and its local Run controls.
    type Origin: Send + Sync;

    /// Capture the installed projection without installing a Session.
    /// `Ok(None)`: nothing is installed. An unreadable foreground record is an
    /// error, which refuses the whole recovery before any Control I/O.
    fn capture_terminal_origin(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Self::Origin>, RunError>;

    /// Retire this generation's completed terminal cleanup projection;
    /// `true` when one was retired.
    async fn retire_completed_terminal_projection(
        &self,
        session_id: &SessionId,
        lease: &SessionRealizationLease,
    ) -> Result<bool, RunError>;

    /// Signal the local Run controls captured in `origin`, if any.
    fn interrupt_origin_runs(&self, origin: &Self::Origin);

    /// Close and retire the projection only while its original phase still
    /// holds this generation (exact or monotonically renewed), signalling the
    /// Run controls captured at that close; a successor generation, a replaced
    /// phase or an absent projection is left untouched.
    async fn retire_origin_generation(
        &self,
        session_id: &SessionId,
        lease: &SessionRealizationLease,
        origin: &Self::Origin,
    ) -> Result<(), RunError>;

    /// Revoke the projection of an assignment another actor settled: it holds
    /// no ordinary Run authority.
    async fn revoke_settled_projection(&self, session_id: &SessionId) -> Result<(), RunError>;
}

/// The participants one terminal recovery runs over, given at the call: the
/// Control that owns the queue, the executor roles the terminal driver needs,
/// and the local projections.
pub struct TerminalRecoveryParticipants<'a, P: ?Sized> {
    pub control: &'a dyn SessionTerminalCleanupControl,
    pub realization: &'a dyn RealizationRuntime,
    pub effects: &'a dyn RealizationEffects,
    pub cleanup: &'a dyn TerminalCleanup,
    pub projections: &'a P,
}

impl<P: ?Sized> Clone for TerminalRecoveryParticipants<'_, P> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<P: ?Sized> Copy for TerminalRecoveryParticipants<'_, P> {}

/// What one terminal reconciliation found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalReconciliation {
    /// The terminal fence owns the projection: the drive ran, a completed
    /// projection was retired, or Control no longer knows the Session.
    Fenced,
    /// Another actor settled the assignment before this recovery observed it.
    SettledElsewhere,
}

/// Why one terminal recovery remained pending.
#[derive(Debug, thiserror::Error)]
pub enum TerminalRecoveryError {
    /// Control could not report the Session's terminal cleanup work.
    #[error("terminal cleanup control remained pending: {0}")]
    Control(SessionRealizationControlFailure),
    /// The terminal cleanup drive did not complete.
    #[error("terminal cleanup remained pending: {0}")]
    Drive(SessionRealizationDriveError),
    /// A process-local projection effect failed.
    #[error("{}", .0.message)]
    Projection(RunError),
    /// The claim cursor did not advance: an older Coordinator ignored it. The
    /// sweep never drives the same cleanup twice and retries after Control and
    /// Worker versions converge.
    #[error("terminal cleanup claim cursor did not advance")]
    ClaimCursorStalled,
    /// Control could not claim the next terminal cleanup assignment.
    #[error("cold Session terminal cleanup claim remained pending: {0}")]
    Claim(SessionRealizationControlFailure),
}

/// The first failure of a recovery sweep and the Session it belongs to
/// (`None` for a claim failure).
#[derive(Debug)]
pub struct TerminalRecoverySweepError {
    pub session_id: Option<SessionId>,
    pub error: TerminalRecoveryError,
}

impl std::fmt::Display for TerminalRecoverySweepError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.session_id {
            Some(session_id) => write!(
                formatter,
                "Session `{session_id}` terminal cleanup remained pending: {}",
                self.error
            ),
            None => self.error.fmt(formatter),
        }
    }
}

impl std::error::Error for TerminalRecoverySweepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Reconcile one assignment against the aggregate's terminal cleanup work.
///
/// Decision table (tests `terminal_cleanup_recovery::tests`):
/// | Rule | Control reports | completed projection retired | origin | Effect |
/// | R1 | work | - | - | drive it through the terminal driver; Fenced |
/// | R2 | no work | yes | - | Fenced |
/// | R3 | no work | no | - | SettledElsewhere |
/// | T1 | NotFound | yes | captured | signal the origin's Run controls; Fenced |
/// | T2 | NotFound | no | captured | retire the origin's generation if it still holds it; Fenced |
/// | T3 | NotFound | no | none | nothing; Fenced |
/// | R7 | another failure | - | - | Control error, no local effect |
///
/// A NotFound never interrupts by Session id: a successor may now serve that
/// same Session, so only the controls captured before the lookup (T1) or at
/// the generation's own close (T2) are signalled.
pub async fn reconcile_terminal_cleanup<P>(
    session_id: &SessionId,
    lease: &SessionRealizationLease,
    participants: TerminalRecoveryParticipants<'_, P>,
) -> Result<TerminalReconciliation, TerminalRecoveryError>
where
    P: TerminalRecoveryProjections + ?Sized,
{
    let projections = participants.projections;
    // A delayed NotFound belongs to this original lookup and physical
    // projection, so the origin is captured before the lookup.
    let origin = projections
        .capture_terminal_origin(session_id)
        .map_err(TerminalRecoveryError::Projection)?;
    match participants
        .control
        .terminal_cleanup_work(session_id, lease)
        .await
    {
        Ok(Some(_)) => {
            // The driver re-reads the closed action and owns installation,
            // effect ordering, receipt admission and acknowledgement.
            crate::drive_session_terminal_cleanup(
                session_id,
                lease,
                participants.control,
                participants.realization,
                participants.effects,
                participants.cleanup,
            )
            .await
            .map_err(TerminalRecoveryError::Drive)?;
            Ok(TerminalReconciliation::Fenced)
        }
        Ok(None) => {
            if projections
                .retire_completed_terminal_projection(session_id, lease)
                .await
                .map_err(TerminalRecoveryError::Projection)?
            {
                Ok(TerminalReconciliation::Fenced)
            } else {
                Ok(TerminalReconciliation::SettledElsewhere)
            }
        }
        Err(SessionRealizationControlFailure::NotFound) => {
            if projections
                .retire_completed_terminal_projection(session_id, lease)
                .await
                .map_err(TerminalRecoveryError::Projection)?
            {
                if let Some(origin) = &origin {
                    projections.interrupt_origin_runs(origin);
                }
            } else if let Some(origin) = &origin {
                projections
                    .retire_origin_generation(session_id, lease, origin)
                    .await
                    .map_err(TerminalRecoveryError::Projection)?;
            }
            Ok(TerminalReconciliation::Fenced)
        }
        Err(error) => Err(TerminalRecoveryError::Control(error)),
    }
}

/// Claim every currently discoverable terminal assignment for `target` and
/// reconcile at most [`MAX_CONCURRENT_TERMINAL_RECOVERIES`] Sessions at a
/// time, at most [`MAX_ASSIGNMENTS_PER_RECOVERY_SWEEP`] per sweep. A completed
/// Session admits the next claim without waiting for a slow sibling. The
/// claim is routing and ownership input; the driver alone installs the
/// aggregate's readback. This is the sole recovery selector for
/// exact-current continuation and expired-lease takeover; ordinary terminal
/// settlement stays on the initiating command path. The caller awaits the
/// whole sweep without imposing a timeout on its durable effects.
///
/// Decision table (tests `terminal_cleanup_recovery::tests`):
/// | Rule | Cause | Effect |
/// | S1 | claims until Control has none | each reconciled; count returned |
/// | S2 | more than the sweep bound | only the bound is claimed |
/// | S3 | more than the concurrency bound | at most that many in flight |
/// | S4 | the claim cursor does not advance | claiming stops; ClaimCursorStalled; claimed work finishes |
/// | S5 | a claim fails | claiming stops; Claim error; claimed work finishes |
/// | S6 | another actor settled an assignment | its projection is revoked |
/// | S7 | one Session fails | every claimed Session still runs; the first failure is returned with its Session |
pub async fn recover_terminal_cleanup_assignments<P>(
    target: SessionRealizationTarget,
    participants: TerminalRecoveryParticipants<'_, P>,
) -> Result<usize, TerminalRecoverySweepError>
where
    P: TerminalRecoveryProjections + ?Sized,
{
    let mut recovered = 0;
    let mut first_error: Option<TerminalRecoverySweepError> = None;
    let mut claims_exhausted = false;
    let mut after_session_id: Option<SessionId> = None;
    let mut in_flight = futures_util::stream::FuturesUnordered::new();
    loop {
        while in_flight.len() < MAX_CONCURRENT_TERMINAL_RECOVERIES
            && recovered < MAX_ASSIGNMENTS_PER_RECOVERY_SWEEP
            && !claims_exhausted
        {
            match participants
                .control
                .claim_next_terminal_cleanup(target.clone(), after_session_id.as_ref())
                .await
            {
                Ok(Some(assignment)) => {
                    if after_session_id
                        .as_ref()
                        .is_some_and(|previous| assignment.session_id <= *previous)
                    {
                        first_error.get_or_insert(TerminalRecoverySweepError {
                            session_id: None,
                            error: TerminalRecoveryError::ClaimCursorStalled,
                        });
                        claims_exhausted = true;
                        continue;
                    }
                    after_session_id = Some(assignment.session_id.clone());
                    recovered += 1;
                    in_flight.push(async move {
                        let result = reconcile_terminal_cleanup(
                            &assignment.session_id,
                            &assignment.authority.lease,
                            participants,
                        )
                        .await;
                        (assignment.session_id, result)
                    });
                }
                Ok(None) => claims_exhausted = true,
                Err(error) => {
                    first_error.get_or_insert(TerminalRecoverySweepError {
                        session_id: None,
                        error: TerminalRecoveryError::Claim(error),
                    });
                    claims_exhausted = true;
                }
            }
        }
        let Some((session_id, result)) = in_flight.next().await else {
            break;
        };
        let error = match result {
            Ok(TerminalReconciliation::Fenced) => continue,
            Ok(TerminalReconciliation::SettledElsewhere) => {
                // Another actor completed the operation between claim and
                // poll; this projection has no ordinary Run authority.
                match participants
                    .projections
                    .revoke_settled_projection(&session_id)
                    .await
                {
                    Ok(()) => continue,
                    Err(error) => TerminalRecoveryError::Projection(error),
                }
            }
            Err(error) => error,
        };
        // The sweep keeps its first failure, but another pending Session must
        // not hide this assignment's own cause from diagnostics.
        tracing::warn!(
            session_id = %session_id,
            %error,
            "cold Session terminal cleanup assignment remained pending"
        );
        first_error.get_or_insert(TerminalRecoverySweepError {
            session_id: Some(session_id),
            error,
        });
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(recovered),
    }
}

#[cfg(test)]
mod tests;
