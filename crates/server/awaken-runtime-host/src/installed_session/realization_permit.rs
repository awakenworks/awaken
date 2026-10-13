// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex
//! Worker-side identity source for the canonical Session realization permit.
//!
//! `InstalledSession::realization` remains the sole mutex and the sole
//! exclusion identity on this side: it is the same `Arc` allocation the slot's
//! revocation selection compares, so a removed/recreated slot is automatically
//! a replaced registration. This adapter only feeds that identity and its
//! acquired owner to the contract-owned kernel and constructor
//! (`awaken_session_realization_contract::SessionRealizationPermit`); it is not another
//! lock, generation counter, registry, admission rule, or durable authority.

use std::sync::Arc;

use awaken_session_realization_contract::SessionRealizationPermit;

use super::{InstalledSession, InstalledSessions};

impl InstalledSessions {
    /// Acquire the current slot's realization owner and atomically apply the
    /// caller's admission projection. If removal/recreation happened while
    /// awaiting Tokio, the contract kernel retries against the replacement
    /// instead of admitting the stale allocation.
    pub(crate) async fn acquire_realization_with<R>(
        self: &Arc<Self>,
        session: &str,
        mut on_admit: impl FnMut(&mut InstalledSession) -> R,
    ) -> (SessionRealizationPermit, R) {
        let slots = self;
        SessionRealizationPermit::acquire(
            &awaken_session_types::SessionId::from(session),
            move || {
                let identity: Arc<tokio::sync::Mutex<()>> = slots
                    .realizations()
                    .update_realization(session, |r| r.realization.clone());
                async move {
                    let owner = identity.clone().lock_owned().await;
                    (identity, owner)
                }
            },
            |identity| {
                self.modify(session, |slot| {
                    Arc::ptr_eq(&slot.realization.realization, identity).then(|| on_admit(slot))
                })
                .flatten()
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_recreated_slot_is_not_admitted_through_the_removed_allocation() {
        // Owner migration keeps the same Arc registry and three slot locks.
        // Both callers now enter the original with/permit kernel directly;
        // the removed projection used this exact unit admission callback.
        // R1/R2 still cover waiting, removal/recreation and current identity.
        // Cause/effect graph: C1 a driver holds the Session's slot allocation;
        // C2 a second driver waits on that same allocation; C3 the slot is
        // removed and recreated while it waits; C4 the first permit is dropped.
        // E1 C2 waits; E2 after C3+C4 it is admitted only against the *current*
        // allocation. Rules: R1=C1+C2=>E1; R2=C1+C2+C3+C4=>E2.
        let slots = Arc::new(InstalledSessions::default());
        let held = slots.acquire_realization_with("session-a", |_| ()).await.0;
        let waiting = slots
            .acquire_realization_with("session-a", |slot| slot.realization.realization.clone());
        let mut waiting = std::pin::pin!(waiting);
        assert!(futures_util::poll!(waiting.as_mut()).is_pending(), "R1/E1");
        slots.remove("session-a");
        let recreated = slots
            .realizations()
            .update_realization("session-a", |r| r.realization.clone());
        drop(held);
        let (_permit, admitted) = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("R2/E2 the waiter was never admitted");
        assert!(Arc::ptr_eq(&admitted, &recreated), "R2/E2");
    }
}
