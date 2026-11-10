// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex
//! Worker-side identity source for the canonical Session realization permit.
//!
//! `InstalledSession::realization` remains the sole mutex and the sole
//! exclusion identity on this side: it is the same `Arc` allocation the installed Session's
//! revocation selection compares, so a removed/recreated installed Session is automatically
//! a replaced registration. This adapter only feeds that identity and its
//! acquired owner to the contract-owned kernel and constructor
//! (`awaken_session_execution_contract::SessionRealizationPermit`); it is not another
//! lock, generation counter, registry, admission rule, or durable authority.

use std::sync::Arc;

use awaken_session_execution_contract::SessionRealizationPermit;

use crate::realization_error::RealizationLock;

use super::{InstalledSession, InstalledSessions};

impl InstalledSessions {
    /// Acquire the current installed Session's realization owner and atomically apply the
    /// caller's admission projection. If removal/recreation happened while
    /// awaiting Tokio, the contract kernel retries against the replacement
    /// instead of admitting the stale allocation. The permit opens the claimed
    /// install path, so it installs the Session; a poisoned registry is
    /// refused at entry or admission, never read through.
    pub(crate) async fn acquire_realization_with<R>(
        &self,
        session: &str,
        mut on_admit: impl FnMut(&mut InstalledSession) -> R,
    ) -> Result<(SessionRealizationPermit, R), RealizationLock> {
        let installed = self;
        let (permit, admitted) = SessionRealizationPermit::acquire(
            &awaken_session_types::SessionId::from(session),
            move || {
                let phase = installed
                    .install_with(session, |s| s.realization.execution().phase.clone())
                    .ok();
                async move {
                    let owner = match &phase {
                        Some(phase) => Some(phase.clone().lock_owned().await),
                        None => None,
                    };
                    (phase, owner)
                }
            },
            |identity| match identity {
                // The registry refused entry: admit the refusal itself, so the
                // caller receives it instead of a retry against a poisoned lock.
                None => Some(Err(RealizationLock::InstalledSessions)),
                Some(identity) => match self.modify(session, |s| {
                    Arc::ptr_eq(&s.realization.execution().phase, identity).then(|| on_admit(s))
                }) {
                    Ok(admitted) => admitted.flatten().map(Ok),
                    Err(poisoned) => Some(Err(poisoned)),
                },
            },
        )
        .await;
        admitted.map(|admitted| (permit, admitted))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_recreated_installed_session_is_not_admitted_through_the_removed_allocation() {
        // Owner migration keeps the same Arc registry and three installed Session locks.
        // Both callers now enter the original with/permit kernel directly;
        // the removed projection used this exact unit admission callback.
        // R1/R2 still cover waiting, removal/recreation and current identity.
        // Cause/effect graph: C1 a driver holds the installed Session allocation;
        // C2 a second driver waits on that same allocation; C3 the installed Session is
        // removed and recreated while it waits; C4 the first permit is dropped.
        // E1 C2 waits; E2 after C3+C4 it is admitted only against the *current*
        // allocation. Rules: R1=C1+C2=>E1; R2=C1+C2+C3+C4=>E2.
        // R3 a borrowed held permit is valid only for its current allocation
        // and Session. The synchronous check cannot reacquire its held lock;
        // removed/recreated or wrong Session identities are refused.
        let installed_sessions = Arc::new(InstalledSessions::default());
        let held = installed_sessions
            .acquire_realization_with("session-a", |_| ())
            .await
            .expect("installed Sessions")
            .0;
        assert!(
            crate::host::borrowed_permit_is_current(&installed_sessions, "session-a", &held)
                .unwrap(),
            "R3 current held permit"
        );
        assert!(
            !crate::host::borrowed_permit_is_current(&installed_sessions, "session-b", &held)
                .unwrap(),
            "R3 another Session"
        );
        let waiting = installed_sessions.acquire_realization_with("session-a", |installed| {
            installed.realization.execution().phase.clone()
        });
        let mut waiting = std::pin::pin!(waiting);
        assert!(futures_util::poll!(waiting.as_mut()).is_pending(), "R1/E1");
        installed_sessions.remove("session-a");
        let recreated =
            installed_sessions.install_realization("session-a", |r| r.execution().phase.clone());
        assert!(
            !crate::host::borrowed_permit_is_current(&installed_sessions, "session-a", &held)
                .unwrap(),
            "R3 removed allocation cannot authorize replacement"
        );
        drop(held);
        let (permit, admitted) = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("R2/E2 the waiter was never admitted")
            .expect("installed Sessions");
        assert!(Arc::ptr_eq(&admitted, &recreated), "R2/E2");
        assert!(
            crate::host::borrowed_permit_is_current(&installed_sessions, "session-a", &permit)
                .unwrap(),
            "R3 replacement permit"
        );
    }

    #[tokio::test]
    async fn realizer_and_host_permits_share_replacement_validation_and_exclusion() {
        // V1 Host owns A and the realization view waits for A. V2 the installed Session is
        // replaced by B while both B and A are held. V3 releasing only A must
        // not admit the realization waiter; it must retry and wait for B.
        // V4 the admitted realizer excludes a Host driver on B until dropped.
        let installed_sessions = Arc::new(InstalledSessions::default());
        let realizations = installed_sessions.realizations();
        let held_a = installed_sessions
            .acquire_realization_with("session-a", |_| ())
            .await
            .expect("installed Sessions")
            .0;
        let mut waiting = std::pin::pin!(realizations.acquire_realization("session-a"));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending(), "V1");
        installed_sessions.remove("session-a");
        let held_b = installed_sessions
            .acquire_realization_with("session-a", |_| ())
            .await
            .expect("installed Sessions")
            .0;
        drop(held_a);
        assert!(futures_util::poll!(waiting.as_mut()).is_pending(), "V3");
        drop(held_b);
        let realizer = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("V3 retry must acquire the replacement allocation")
            .expect("installed Sessions");
        assert_eq!(realizer.session_id().as_str(), "session-a");
        let host = installed_sessions.acquire_realization_with("session-a", |_| ());
        let mut host = std::pin::pin!(host);
        assert!(futures_util::poll!(host.as_mut()).is_pending(), "V4");
        drop(realizer);
        tokio::time::timeout(std::time::Duration::from_secs(5), host)
            .await
            .expect("V4 the realization permit must release the Host driver")
            .expect("installed Sessions");
    }
}
