//! Worker-side identity source for the canonical Session realization permit.
//!
//! `SessionRuntimeSlot::realization` remains the sole mutex and the sole
//! exclusion identity on this side: it is the same `Arc` allocation the slot's
//! revocation selection compares, so a removed/recreated slot is automatically
//! a replaced registration. This adapter only feeds that identity and its
//! acquired owner to the contract-owned kernel and constructor
//! (`awaken_session_contract::SessionRealizationPermit`); it is not another
//! lock, generation counter, registry, admission rule, or durable authority.

use std::sync::Arc;

use awaken_session_contract::SessionRealizationPermit;

use super::{SessionRuntimeSlot, SessionRuntimeSlots};

impl SessionRuntimeSlots {
    /// Acquire the current slot's realization owner and atomically apply the
    /// caller's admission projection. If removal/recreation happened while
    /// awaiting Tokio, the contract kernel retries against the replacement
    /// instead of admitting the stale allocation.
    pub(crate) async fn acquire_realization_with<R>(
        &self,
        session: &str,
        mut on_admit: impl FnMut(&mut SessionRuntimeSlot) -> R,
    ) -> (SessionRealizationPermit, R) {
        let slots = self;
        SessionRealizationPermit::acquire(
            &awaken_session_contract::SessionId::from(session),
            move || {
                let identity: Arc<tokio::sync::Mutex<()>> =
                    slots.update(session, |slot| slot.realization.clone());
                async move {
                    let owner = identity.clone().lock_owned().await;
                    (identity, owner)
                }
            },
            |identity| {
                self.modify(session, |slot| {
                    Arc::ptr_eq(&slot.realization, identity).then(|| on_admit(slot))
                })
                .flatten()
            },
        )
        .await
    }

    pub(crate) async fn acquire_realization(&self, session: &str) -> SessionRealizationPermit {
        self.acquire_realization_with(session, |_| ()).await.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_recreated_slot_is_not_admitted_through_the_removed_allocation() {
        // Cause/effect graph: C1 a driver holds the Session's slot allocation;
        // C2 a second driver waits on that same allocation; C3 the slot is
        // removed and recreated while it waits; C4 the first permit is dropped.
        // E1 C2 waits; E2 after C3+C4 it is admitted only against the *current*
        // allocation. Rules: R1=C1+C2=>E1; R2=C1+C2+C3+C4=>E2.
        let slots = SessionRuntimeSlots::default();
        let held = slots.acquire_realization("session-a").await;
        let waiting = slots.acquire_realization_with("session-a", |slot| slot.realization.clone());
        let mut waiting = std::pin::pin!(waiting);
        assert!(futures_util::poll!(waiting.as_mut()).is_pending(), "R1/E1");
        slots.remove("session-a");
        let recreated = slots.update("session-a", |slot| slot.realization.clone());
        drop(held);
        let (_permit, admitted) = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("R2/E2 the waiter was never admitted");
        assert!(Arc::ptr_eq(&admitted, &recreated), "R2/E2");
    }
}
