//! Lease-only scheduling and response application for the canonical Session slot.

use super::realization_control_deadline::DeadlineSessionRealizationControl;
use super::{RenewalFailureDisposition, realization_renewal_failure_disposition};
use awaken_runtime_contract::authority_lease::AuthorityLeaseTiming;
use futures_util::stream::{self, StreamExt};
use std::sync::Arc;

/// Independent Session lease renewals are fast root CAS operations. Bound them
/// per Worker so cloud replica count scales total capacity without removing
/// local backpressure or allowing one Worker to overload Control.
pub(crate) const MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS: usize = 8;

/// Scoped preparation, not another lease manager. The existing scheduler sees
/// the canonical slot before Resource I/O; this guard only bounds that I/O's
/// lifetime and removes a newly introduced, unpublished lease on failure/drop.
pub(super) struct ResourcePreparation<'a> {
    host: &'a crate::SharedHost,
    session: &'a str,
    asserted: awaken_session_contract::SessionRealizationLease,
    cancellation: Arc<awaken_runtime_contract::CancellationToken>,
    introduced_lease: bool,
}

impl<'a> ResourcePreparation<'a> {
    pub(super) fn begin(
        host: &'a crate::SharedHost,
        session: &'a str,
        proof: awaken_session_contract::ObservedSessionRealizationLease,
    ) -> Result<Self, crate::HostError> {
        let asserted = proof.lease.clone();
        let cancellation = Arc::new(awaken_runtime_contract::CancellationToken::new());
        let (introduced_lease, changed) = host.authorize_worker_effect(|| {
            host.session_slots.update(session, |slot| {
                if slot.resource_preparation.is_some() {
                    return Err(crate::HostError::internal(
                        "Resource preparation already holds its projection lock",
                    ));
                }
                let introduced = slot.realization_authority.lease().is_none();
                if !slot.install_realization_lease(proof) {
                    return Err(Self::ended());
                }
                slot.resource_preparation = Some(cancellation.clone());
                Ok((introduced, slot.realization_changed.clone()))
            })
        })?;
        changed.notify_waiters();
        let preparation = Self {
            host,
            session,
            asserted,
            cancellation,
            introduced_lease,
        };
        preparation.validate()?;
        Ok(preparation)
    }

    fn authorized(&self, slot: &crate::session_slot::SessionRuntimeSlot) -> bool {
        slot.resource_preparation
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.cancellation))
            && slot.resource_preparation_authorized(&self.asserted)
    }

    fn ended() -> crate::HostError {
        crate::HostError::internal(
            "Session realization authority ended during Resource preparation",
        )
    }

    pub(super) fn validate(&self) -> Result<(), crate::HostError> {
        if self
            .host
            .session_slots
            .read(self.session, |slot| self.authorized(slot))
            .unwrap_or(false)
        {
            Ok(())
        } else {
            Err(Self::ended())
        }
    }

    pub(super) fn finish(
        &self,
        slot: &mut crate::session_slot::SessionRuntimeSlot,
    ) -> Result<(), crate::HostError> {
        if !self.authorized(slot) {
            return Err(Self::ended());
        }
        slot.resource_preparation = None;
        Ok(())
    }

    pub(super) async fn authority_ended(&self) -> crate::HostError {
        loop {
            let Some(changed) = self
                .host
                .session_slots
                .read(self.session, |slot| slot.realization_changed.clone())
            else {
                return Self::ended();
            };
            // Register before reading the lease to avoid a lost renewal wakeup.
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let remaining = self
                .host
                .session_slots
                .read(self.session, |slot| {
                    self.authorized(slot)
                        .then(|| slot.realization_authority.remaining())
                        .flatten()
                })
                .flatten();
            let Some(remaining) = remaining else {
                return Self::ended();
            };
            tokio::select! {
                () = self.cancellation.cancelled() => return Self::ended(),
                () = tokio::time::sleep(remaining) => {},
                () = &mut notified => {},
            }
        }
    }
}

impl Drop for ResourcePreparation<'_> {
    fn drop(&mut self) {
        self.host.session_slots.modify(self.session, |slot| {
            if slot
                .resource_preparation
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.cancellation))
            {
                slot.resource_preparation = None;
                if self.introduced_lease
                    && slot.realization_authority.lease().is_some_and(|current| {
                        awaken_session_contract::realization_lease_generation_authorizes(
                            current,
                            &self.asserted,
                        )
                    })
                {
                    slot.realization_authority =
                        crate::session_slot::SessionRealizationAuthority::Uninstalled;
                    slot.realization_changed.notify_waiters();
                }
            }
        });
    }
}

impl crate::SharedHost {
    async fn reconcile_one_session_realization(
        &self,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: String,
        lease: awaken_session_contract::SessionRealizationLease,
        requested_expiry_unix_ms: u64,
    ) -> Result<bool, crate::HostError> {
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
        // Capture the existing slot identity before Control I/O. Equal lease
        // fields do not authorize an ABA replacement of the Session slot.
        let Some(realization) = self
            .session_slots
            .read(&session_id, |slot| {
                (slot.realization_lease.as_ref() == Some(&lease)).then(|| slot.realization.clone())
            })
            .flatten()
        else {
            return Ok(false);
        };
        let renewal: Result<bool, (crate::HostError, bool)> = async {
            let control_failure = |error| {
                let revoke = realization_renewal_failure_disposition(&error)
                    == RenewalFailureDisposition::RevokeImmediately;
                (crate::HostError::internal(error.to_string()), revoke)
            };
            let request_started = awaken_local_process::authority_proof_started()
                .map_err(|error| (crate::HostError::internal(error.to_string()), true))?;
            let renewed_lease = match control
                .renew_session_realization(awaken_session_contract::RenewSessionRealization {
                    session_id: session_id.clone(),
                    asserted_lease: lease.clone(),
                    requested_expires_at_unix_ms: requested_expiry_unix_ms,
                })
                .await
            {
                Ok(renewed_lease) => renewed_lease.observed(request_started),
                Err(error) => return Err(control_failure(error)),
            };
            // The response belongs to the snapshot sent before I/O. Compare
            // and install atomically: another driver may already have extended,
            // replaced, or removed that projection. A renewal is not authority
            // to recreate a slot or overwrite a later installed lease.
            let applied = self
                .authorize_worker_effect(|| {
                    Ok(self
                        .session_slots
                        .modify(&session_id, |slot| {
                            Arc::ptr_eq(&slot.realization, &realization)
                                 && slot.renew_realization_lease(&lease, renewed_lease)
                        })
                        .unwrap_or(false))
                })
                .map_err(|error| (error, true))?;
            Ok(applied)
        }
        .await;
        match renewal {
            Ok(false)
                if self
                    .session_slots
                    .read(&session_id, |slot| {
                        slot.realization_authority.lease() == Some(&lease)
                            && slot.realization_authority.remaining().is_none()
                    })
                    .unwrap_or(false) =>
            {
                self.revoke_session_realization(&session_id, Some(&lease))
                    .await?;
                Ok(false)
            }
            Ok(applied) => Ok(applied),
            Err(_)
                if !self
                    .session_slots
                    .read(&session_id, |slot| {
                        Arc::ptr_eq(&slot.realization, &realization)
                            && slot.realization_authority.lease() == Some(&lease)
                    })
                    .unwrap_or(false) =>
            {
                Ok(false)
            }
            Err((error, revoke_immediately))
                if !revoke_immediately
                    && self
                        .session_slots
                        .read(&session_id, |slot| {
                            slot.realization_authority.remaining().is_some()
                        })
                        .unwrap_or(false) =>
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
            self.session_control
                .as_deref()
                .map(|control| control as &dyn awaken_session_contract::SessionRealizationControl),
            None,
            timing,
        )
        .await
    }

    pub(crate) async fn renew_session_realization_projections(
        &self,
        control: Option<&dyn awaken_session_contract::SessionRealizationControl>,
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
        let outcomes = stream::iter(self.session_slots.execution_environments())
            .map(|environment| async move {
                let crate::session_environment::SessionEnvironment::Workdir { sandbox, .. } =
                    environment.as_ref()
                else {
                    unreachable!()
                };
                sandbox
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
        control: Option<&dyn awaken_session_contract::SessionRealizationControl>,
        owner: Option<(&str, &str)>,
        timing: AuthorityLeaseTiming,
    ) -> Result<usize, crate::HostError> {
        let now = awaken_local_process::authority_proof_started()
            .map_err(|error| crate::HostError::internal(error.to_string()))?;
        let mut due = self
            .session_slots
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
            awaken_session_contract::SessionRealizationLease,
            std::time::Duration,
        ),
                     right: &(
            String,
            awaken_session_contract::SessionRealizationLease,
            std::time::Duration,
        )| { left.2.cmp(&right.2).then_with(|| left.0.cmp(&right.0)) };
        due.sort_by(order);
        let outcomes = stream::iter(due)
            .map(|(session_id, lease, _)| {
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
            })
            .buffer_unordered(MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS)
            .collect::<Vec<_>>()
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
