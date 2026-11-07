// @formal surface obligation=session_execution.one_physical_attempt_per_thread
// @formal reason: Resolves the exact claim-fenced reservation before a Worker may attempt execution.
//! Claim-fenced recovery of a reserved Session Run before execution can begin.

use std::sync::Arc;

use super::{ClaimFuture, ExecutionAuthority};
use crate::dispatch::{Claimed, SessionRunReservationResolution};
use crate::{DispatchSettlementObserver, SessionRunAdmissionObserver};
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_dispatch_erasure::Dispatch;
use awaken_session_execution_contract::{
    SessionRunActivityAdmission, SessionRunActivityAdmissionMode,
};
use awaken_time::WallClock;
use awaken_worker_contract::RunClaim;

#[derive(Default)]
pub(super) struct SessionRunReservationRecovery {
    admission_observer: Option<Arc<dyn SessionRunAdmissionObserver>>,
}

pub(super) struct ReservationRecoveryContext<'a, S> {
    pub(super) store: &'a S,
    pub(super) execution_authority: &'a ExecutionAuthority,
    pub(super) settlement_observer: Option<&'a dyn DispatchSettlementObserver>,
    pub(super) recovery_source: Option<&'a dyn RunRecoverySource>,
    pub(super) reservation_ttl_ms: u64,
}

impl SessionRunReservationRecovery {
    pub(super) fn set_admission_observer(
        &mut self,
        observer: Arc<dyn SessionRunAdmissionObserver>,
    ) {
        self.admission_observer = Some(observer);
    }

    pub(super) fn recover<'a, S: Dispatch + 'static>(
        &'a self,
        claimed: &'a Claimed,
        clock: &'a dyn WallClock,
        context: ReservationRecoveryContext<'a, S>,
    ) -> ClaimFuture<'a, ()> {
        // Cause/effect decision table for an expired Session Run reservation:
        // C1=claim still current; C2=cancelled; C3=Session authority decision.
        // E1=delete unstarted reserved dispatch; E2=bind epoch and publish ordinary Pending;
        // E3=return to Reserved for retry; E4=stale owner changes nothing.
        //
        // | Rule | C1 | C2 | C3 | Queue effect | Executor |
        // |---|---|---|---|---|---|
        // | R1 | T | T | receipt absent | E1 Rejected | never entered |
        // | R2 | T | T | receipt present | E2 Pending(cancelled) | later cancellation claim |
        // | R3 | T | F | admitted | E2 Pending | later ordinary claim |
        // | R4 | T | F | rejected | E1 Rejected | never entered |
        // | R5 | T | any | unavailable | E3 Reserved | never entered |
        // | R6 | F | any | any | E4 Fenced | never entered |
        Box::pin(async move {
            let run_id = claimed.request.run_id();
            let claim = RunClaim::from(&claimed.lease);
            let observer = self.admission_observer.as_deref();
            let observer_owns_fence = context
                .settlement_observer
                .is_some_and(|observer| observer.owns_dispatch_fence());
            let prepared_kind = claimed.request.identity_scope
                == awaken_dispatch_contract::DispatchIdentityScope::PreparedSessionCommand;
            let preparation = match observer {
                Some(observer) if prepared_kind && !observer_owns_fence => {
                    observer
                        .prepare_session_run_activity(
                            &claimed.request,
                            &claim,
                            if claimed.cancellation_requested {
                                SessionRunActivityAdmissionMode::RecoverOnly
                            } else {
                                SessionRunActivityAdmissionMode::RecoverOrAdmit
                            },
                        )
                        .await
                }
                _ => Ok(None),
            };
            let mut local_guard = context
                .execution_authority
                .reservation_guard(&claim, observer.is_some() && !observer_owns_fence)
                .await?;
            let cancellation_requested = local_guard
                .as_ref()
                .map_or(claimed.cancellation_requested, |guard| {
                    guard.cancellation_requested()
                });
            let mode = if cancellation_requested {
                SessionRunActivityAdmissionMode::RecoverOnly
            } else {
                SessionRunActivityAdmissionMode::RecoverOrAdmit
            };
            let resolution = if prepared_kind && !observer_owns_fence {
                match local_guard.take() {
                    Some(guard) => {
                        let (projected_revision, recovery) = match preparation {
                            Ok(revision) => {
                                let recovery = if revision.is_some() {
                                    match context.recovery_source {
                                        Some(source) => Ok(awaken_dispatch_contract::PreparedRunRecovery::OriginalAuthority(source)),
                                        None => Err(awaken_dispatch_contract::DispatchSettlementError(
                                            "prepared reservation requires its guarded original recovery source".into(),
                                        )),
                                    }
                                } else {
                                    Ok(awaken_dispatch_contract::PreparedRunRecovery::ReceiptOnly)
                                };
                                (revision, recovery)
                            }
                            Err(error) => (None, Err(error)),
                        };
                        let recovery = recovery.and_then(|source| {
                            if guard.request().same_original_dispatch(&claimed.request) {
                                Ok(source)
                            } else {
                                Err(awaken_dispatch_contract::DispatchSettlementError(
                                    "prepared reservation changed during preparation".into(),
                                ))
                            }
                        });
                        let dispatch = &claimed.request;
                        let admission_claim = &claim;
                        guard
                            .admit_prepared_session_run(
                                clock,
                                recovery,
                                |cursor, cancelled| async move {
                                    match observer {
                                        Some(observer) => observer
                                            .admit_session_run_activity(
                                                dispatch,
                                                admission_claim,
                                                if cancelled {
                                                    SessionRunActivityAdmissionMode::RecoverOnly
                                                } else {
                                                    SessionRunActivityAdmissionMode::RecoverOrAdmit
                                                },
                                                Some(awaken_session_execution_contract::PreparedSessionRunActivity {
                                                    projected_revision,
                                                    admitted_after_cursor: cursor,
                                                }),
                                            )
                                            .await,
                                        None => Err(awaken_dispatch_contract::DispatchSettlementError(
                                            "Session Run activity admission observer is not installed".into(),
                                        )),
                                    }
                                },
                                awaken_dispatch_contract::DispatchSettlementError,
                            )
                            .await
                            .map_err(|error| crate::Error::Dispatch(crate::DispatchError::unavailable(error)))?
                    }
                    None => match preparation {
                        Err(error) => Err(error),
                        Ok(_) => Err(awaken_dispatch_contract::DispatchSettlementError(
                            if observer.is_some() {
                                "Session Run reservation claim is stale"
                            } else {
                                "Session Run activity admission observer is not installed"
                            }
                            .into(),
                        )),
                    },
                }
            } else {
                match (preparation, observer) {
                    (Err(error), _) => Err(error),
                    (Ok(projected_revision), Some(observer))
                        if observer_owns_fence || local_guard.is_some() =>
                    {
                        observer
                            .admit_session_run_activity(
                                &claimed.request,
                                &claim,
                                mode,
                                prepared_kind.then_some(
                                    awaken_session_execution_contract::PreparedSessionRunActivity {
                                        projected_revision,
                                        admitted_after_cursor: 0,
                                    },
                                ),
                            )
                            .await
                    }
                    (Ok(_), Some(_)) => Err(awaken_dispatch_contract::DispatchSettlementError(
                        "Session Run reservation claim is stale".into(),
                    )),
                    (Ok(_), None) => Err(awaken_dispatch_contract::DispatchSettlementError(
                        "Session Run activity admission observer is not installed".into(),
                    )),
                }
            };
            // The common prepared composition has already released its guard.
            // Ordinary/local repair retains the original explicit boundary.
            if let Some(guard) = local_guard {
                guard.release().await.map_err(|error| {
                    crate::Error::Dispatch(crate::DispatchError::unavailable(error))
                })?;
            }
            match resolution {
                Ok(SessionRunActivityAdmission::Admitted {
                    session_activity_epoch,
                }) => {
                    context
                        .store
                        .resolve_claimed_session_run_reservation(
                            &claim,
                            SessionRunReservationResolution::Admitted {
                                session_activity_epoch,
                            },
                        )
                        .await?;
                }
                Ok(SessionRunActivityAdmission::Rejected) => {
                    context
                        .store
                        .resolve_claimed_session_run_reservation(
                            &claim,
                            SessionRunReservationResolution::Rejected,
                        )
                        .await?;
                }
                Err(error) => {
                    context
                        .store
                        .resolve_claimed_session_run_reservation(
                            &claim,
                            SessionRunReservationResolution::Retry {
                                reservation_ttl_ms: context.reservation_ttl_ms,
                            },
                        )
                        .await?;
                    tracing::warn!(
                        run_id = %run_id.0,
                        %error,
                        "Session Run reservation admission deferred"
                    );
                }
            }
            Ok(())
        })
    }
}
