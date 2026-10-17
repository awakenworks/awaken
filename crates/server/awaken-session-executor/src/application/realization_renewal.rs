// @formal entrypoint session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Lease-only scheduling and response application for the canonical Session slot.

use awaken_lease_contract::authority::AuthorityLeaseTiming;
use awaken_session_realization_contract::ObservedSessionRealizationLease;
use awaken_session_realization_contract::SessionRealizationControl as _;
use awaken_session_realization_driver::DeadlineSessionRealizationControl;
#[cfg(any(target_os = "linux", test))]
use awaken_session_realization_driver::MAX_CONCURRENT_SESSION_REALIZATION_REQUESTS;
#[cfg(target_os = "linux")]
use futures_util::stream::{self, StreamExt};
use std::sync::Arc;

/// Independent Session lease renewals are fast root CAS operations. Bound them
/// per Worker so cloud replica count scales total capacity without removing
/// local backpressure or allowing one Worker to overload Control.
#[cfg(any(target_os = "linux", test))]
pub(crate) const MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS: usize =
    MAX_CONCURRENT_SESSION_REALIZATION_REQUESTS;

impl crate::SharedHost {
    #[tracing::instrument(name = "session.realization.renew", skip_all, fields(session_id, epoch = lease.epoch, owner = %lease.owner, incarnation = %lease.runtime_incarnation))]
    async fn reconcile_one_session_realization(
        &self,
        control: &DeadlineSessionRealizationControl<'_>,
        session_id: String,
        lease: crate::session_resource::SessionRealizationLease,
        requested_expiry_unix_ms: u64,
    ) -> Result<bool, crate::HostError> {
        let realizations = self.installed_sessions.realizations();
        // Cause/effect decision table: C1 this lease was selected by the due-only
        // scheduler; C2 Control extends its exact generation; C3 Control
        // explicitly rejects this owner; C4 Control is temporarily unavailable;
        // C5 the old lease remains live. Effects: E1 install the monotonic lease;
        // E2 interrupt and revoke immediately; E3 retain and retry; E4 interrupt
        // and revoke at expiry. Terminal work is intentionally absent: the
        // initiating command owns ordinary settlement and the Worker
        // lifecycle's cold claim-next lane is the sole recovery scheduler.
        //
        // | Rule | due | Control | old lease | Effect |
        // All rules require the asserted projection to remain current at
        // completion. A changed/removed projection discards the obsolete reply;
        // revocation rechecks after its owner locks before interrupting a Run.
        // | R1 | yes | renewed/live response | live | E1 |
        // | R1b | yes | renewed | expired | E4 (never revive) |
        // | R1c | yes | expired response | live | no install; retry |
        // | R2 | yes | explicit loss | any | E2 |
        // | R3 | yes | temporary failure | live | E3 |
        // | R4 | yes | temporary failure | expired | E4 |
        // The original success/error arms are carried by Result::map/map_err;
        // R1-R4 keep the same observation time and error/revocation policy.
        // Capture the existing slot identity before Control I/O. Equal lease
        // fields do not authorize an ABA replacement of the Session slot.
        let Some(realization) = self
            .installed_sessions
            .read(&session_id, |slot| {
                (slot.realization.execution().authority.lease() == Some(&lease))
                    .then(|| slot.realization.execution().phase.clone())
            })
            .flatten()
        else {
            return Ok(false);
        };
        let renewal: Result<ObservedSessionRealizationLease, (crate::HostError, bool)> = async {
            let control_failure =
                |error: awaken_session_realization_contract::SessionRealizationControlFailure| {
                    let revoke = error.proves_current_realization_cannot_continue();
                    (crate::HostError::internal(error.to_string()), revoke)
                };
            let request_started = awaken_authority_clock::authority_proof_started()
                .map_err(|error| (crate::HostError::internal(error.to_string()), true))?;
            control
                .renew_session_realization(
                    awaken_session_realization_contract::RenewSessionRealization {
                        session_id: awaken_session_types::SessionId::from(&session_id),
                        asserted_lease: lease.clone(),
                        requested_expires_at_unix_ms: requested_expiry_unix_ms,
                    },
                )
                .await
                .map(|renewed_lease| renewed_lease.observed(request_started))
                .map_err(control_failure)
        }
        .await;
        // The Control request released its shared permit before reaching this
        // point. Re-enter the same scheduler for local response application or
        // exact-owner cleanup: queued peer requests can consume the released
        // capacity, while no unbounded second completion lane is created.
        let _completion_permit = control
            .acquire_completion_permit()
            .await
            .map_err(|error| crate::HostError::internal(error.to_string()))?;
        let renewal: Result<bool, (crate::HostError, bool)> = match renewal {
            Ok(renewed_lease) => {
                // The response belongs to the snapshot sent before I/O. Compare
                // and install atomically: another driver may already have extended,
                // replaced, or removed that projection. A renewal is not authority
                // to recreate a slot or overwrite a later installed lease.
                self.realization_executor
                    .authority()
                    .authorize_worker_effect::<_, crate::HostError>(|| {
                        Ok(realizations
                            .modify_realization(&session_id, |state| {
                                Arc::ptr_eq(&state.execution().phase, &realization)
                                    && state.renew_lease(&lease, renewed_lease)
                            })?
                            .unwrap_or(false))
                    })
                    .map_err(|error| (error, true))
            }
            Err(error) => Err(error),
        };
        match renewal {
            Ok(false)
                if realizations
                    .read_realization(&session_id, |state| {
                        Arc::ptr_eq(&state.execution().phase, &realization)
                            && state.execution().authority.lease() == Some(&lease)
                            && state.execution().authority.remaining().is_none()
                    })?
                    .unwrap_or(false) =>
            {
                self.revoke_session_realization(&session_id, Some((&lease, &realization)))
                    .await?;
                Ok(false)
            }
            Ok(applied) => Ok(applied),
            Err(_)
                if !realizations
                    .read_realization(&session_id, |state| {
                        Arc::ptr_eq(&state.execution().phase, &realization)
                            && state.execution().authority.lease() == Some(&lease)
                    })?
                    .unwrap_or(false) =>
            {
                Ok(false)
            }
            Err((error, revoke_immediately))
                if !revoke_immediately
                    && realizations
                        .read_realization(&session_id, |r| r.execution().authority.remaining())?
                        .flatten()
                        .is_some() =>
            {
                Err(error)
            }
            Err((error, _)) => {
                if self
                    .revoke_session_realization(&session_id, Some((&lease, &realization)))
                    .await?
                {
                    eprintln!(
                        "Session realization authority ended for `{session_id}`; revoked only its unchanged projection: {error}"
                    );
                }
                Ok(false)
            }
        }
    }

    /// Renew every due Session through the lease-only root CAS. Environment-only
    /// Sessions participate because physical realization can outlive the initial
    /// lease even when no MCP attachment exists. Ordinary terminal settlement
    /// remains on the initiating command path; recovery discovery and effects
    /// remain exclusively on the Worker lifecycle's cold claim-next lane.
    ///
    /// A conclusive ownership loss or expired proof revokes only that Session;
    /// a transient failure retains a still-live lease for the next sweep.
    /// Authority-derived per-request deadlines release occupied capacity, and
    /// earliest-deadline ordering prevents one Session from starving another.
    pub async fn renew_due_session_realizations(
        &self,
        timing: AuthorityLeaseTiming,
    ) -> Result<usize, crate::HostError> {
        self.renew_session_realization_projections(
            self.worker.session_control.as_deref().map(|control| {
                control as &dyn awaken_session_realization_contract::SessionRealizationControl
            }),
            None,
            timing,
        )
        .await
    }

    pub(crate) async fn renew_session_realization_projections(
        &self,
        control: Option<&dyn awaken_session_realization_contract::SessionRealizationControl>,
        owner: Option<(&str, &str)>,
        timing: AuthorityLeaseTiming,
    ) -> Result<usize, crate::HostError> {
        #[cfg(target_os = "linux")]
        {
            // One existing sweep drives BOTH authority renewal and physical
            // cutoff delivery. A long Session lease must not suppress refresh
            // of its shorter Worker proof. Neither lane may starve its peers.
            let (renewal, refresh) = tokio::join!(
                self.renew_due_session_projection_leases(control, owner, timing),
                self.refresh_session_execution(),
            );
            let renewed = renewal?;
            refresh?;
            Ok(renewed)
        }
        #[cfg(not(target_os = "linux"))]
        self.renew_due_session_projection_leases(control, owner, timing)
            .await
    }

    #[cfg(target_os = "linux")]
    async fn refresh_session_execution(&self) -> Result<(), crate::HostError> {
        let outcomes = stream::iter(self.installed_sessions.execution_environments())
            .map(|environment| async move {
                let Some(scoped) = environment.scoped_execution() else {
                    unreachable!("execution environments are exactly the scoped ones")
                };
                scoped
                    .refresh_scoped_execution()
                    .await
                    .map_err(|error| crate::HostError::internal(error.to_string()))
            })
            .buffer_unordered(MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS)
            .collect::<Vec<_>>()
            .await;
        outcomes.into_iter().collect()
    }

    async fn renew_due_session_projection_leases(
        &self,
        control: Option<&dyn awaken_session_realization_contract::SessionRealizationControl>,
        owner: Option<(&str, &str)>,
        timing: AuthorityLeaseTiming,
    ) -> Result<usize, crate::HostError> {
        let now = awaken_authority_clock::authority_proof_started()
            .map_err(|error| crate::HostError::internal(error.to_string()))?;
        let mut due = self
            .installed_sessions
            .realization_leases()
            .into_iter()
            .filter(|(_, proof)| {
                owner.is_none_or(|(owner, incarnation)| {
                    proof.lease.owner == owner && proof.lease.runtime_incarnation == incarnation
                })
            })
            .map(|(session, proof)| {
                let remaining = proof.remaining_at(now).unwrap_or_default();
                (session, proof.lease, remaining)
            })
            .filter(|(_, _, remaining)| *remaining <= timing.renew_interval())
            .collect::<Vec<_>>();
        if due.is_empty() {
            return Ok(0);
        }
        let control = control.ok_or_else(|| {
            crate::HostError::internal("due Session projection has no Control renewal client")
        })?;
        let control = DeadlineSessionRealizationControl::new(control, timing.request_timeout());
        // This cadence owns only due lease writes. The independent lifecycle
        // lane owns cold claim-next recovery and complete effect execution.
        let order = |left: &(
            String,
            crate::session_resource::SessionRealizationLease,
            std::time::Duration,
        ),
                     right: &(
            String,
            crate::session_resource::SessionRealizationLease,
            std::time::Duration,
        )| { left.2.cmp(&right.2).then_with(|| left.0.cmp(&right.0)) };
        due.sort_by(order);
        let outcomes =
            futures_util::future::join_all(due.into_iter().map(|(session_id, lease, _)| {
                let control = &control;
                // The exact asserted expiry is a Control-clock coordinate.
                // Extend that coordinate, never add a Worker wall-clock value.
                let requested_expiry_unix_ms = lease.expires_at_unix_ms.saturating_add(
                    u64::try_from(timing.renew_interval().as_millis()).unwrap_or(u64::MAX),
                );
                async move {
                    self.reconcile_one_session_realization(
                        control,
                        session_id,
                        lease,
                        requested_expiry_unix_ms,
                    )
                    .await
                }
            }))
            .await;
        let mut renewed = 0;
        let mut first_error = None;
        for outcome in outcomes {
            match outcome {
                Ok(true) => renewed += 1,
                Ok(false) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(renewed),
        }
    }
}
