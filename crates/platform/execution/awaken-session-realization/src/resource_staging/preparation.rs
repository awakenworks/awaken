// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Scoped Resource I/O preparation over the installed Session.
//!
//! Worker authorization encloses the original synchronous lease installation.
//! The scope retains no Host or scheduler, and can only remove its own token
//! and a lease it introduced, never a replacement preparation or generation.

use crate::execution_authority::ExecutionAuthority;
use crate::realization_error::RealizationError;
use crate::realization_state::SessionRealization;
use crate::session_realizations::SessionRealizations;
use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_execution_contract::ObservedSessionRealizationLease;
use std::sync::Arc;

/// Scoped preparation, not another lease manager. The existing scheduler sees
/// the installed Session before Resource I/O; this guard only bounds that I/O's
/// lifetime and removes a newly introduced, unpublished lease on failure/drop.
pub struct ResourcePreparation<'a> {
    sessions: SessionRealizations,
    session: &'a str,
    asserted: SessionRealizationLease,
    cancellation: Arc<awaken_runtime_contract::CancellationToken>,
    introduced_lease: bool,
}

impl<'a> ResourcePreparation<'a> {
    pub fn begin(
        authority: &ExecutionAuthority,
        session: &'a str,
        proof: ObservedSessionRealizationLease,
    ) -> Result<Self, RealizationError> {
        let asserted = proof.lease.clone();
        let cancellation = Arc::new(awaken_runtime_contract::CancellationToken::new());
        let sessions = authority.sessions().clone();
        let (introduced_lease, changed) = authority.authorize_worker_effect(|| {
            sessions.update_realization(session, |r| {
                if r.staging().preparation.is_some() {
                    return Err(RealizationError::invariant(
                        "Resource preparation already holds its projection lock",
                    ));
                }
                let introduced = r.execution().authority.lease().is_none();
                if !r.install_lease(proof) {
                    return Err(Self::ended());
                }
                r.staging_mut().preparation = Some(cancellation.clone());
                Ok((introduced, r.execution().changed.clone()))
            })?
        })?;
        changed.notify_waiters();
        let preparation = Self {
            sessions,
            session,
            asserted,
            cancellation,
            introduced_lease,
        };
        preparation.validate()?;
        Ok(preparation)
    }

    fn authorized(&self, realization: &SessionRealization) -> bool {
        realization
            .staging()
            .preparation
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.cancellation))
            && realization.resource_preparation_authorized(&self.asserted)
    }

    fn ended() -> RealizationError {
        RealizationError::authority_lost(
            crate::realization_error::AuthorityLoss::Closed,
            "Session realization authority ended during Resource preparation",
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_cancellation(&self) -> &Arc<awaken_runtime_contract::CancellationToken> {
        &self.cancellation
    }

    pub fn validate(&self) -> Result<(), RealizationError> {
        self.sessions
            .read_realization(self.session, |realization| self.authorized(realization))?
            .filter(|authorized| *authorized)
            .map(|_| ())
            .ok_or_else(Self::ended)
    }

    pub fn finish(&self, realization: &mut SessionRealization) -> Result<(), RealizationError> {
        if !self.authorized(realization) {
            return Err(Self::ended());
        }
        realization.staging_mut().preparation = None;
        Ok(())
    }

    pub async fn authority_ended(&self) -> RealizationError {
        let realizations = &self.sessions;
        loop {
            let changed = match realizations
                .read_realization(self.session, |r| r.execution().changed.clone())
            {
                Ok(Some(changed)) => changed,
                Ok(None) => return Self::ended(),
                Err(denied) => return denied,
            };
            // Register before reading the lease to avoid a lost renewal wakeup.
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let remaining = match realizations.read_realization(self.session, |realization| {
                self.authorized(realization)
                    .then(|| realization.execution().authority.remaining())
                    .flatten()
            }) {
                Ok(remaining) => remaining.flatten(),
                Err(denied) => return denied,
            };
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
        let realizations = &self.sessions;
        // Drop cannot report: a poisoned or retired registry holds no
        // preparation of this scope left to release.
        let _ = realizations.modify_realization(self.session, |r| {
            r.end_resource_preparation(
                &self.cancellation,
                self.introduced_lease.then_some(&self.asserted),
            );
        });
    }
}

#[cfg(test)]
mod tests;
