//! Canonical typed permit for the process-local Session realization driver.
//!
//! `SessionRuntimeSlot::realization` remains the sole mutex. This adapter makes
//! its ownership explicit at every production driver entrance and rejects an
//! acquired stale `Arc` after slot removal/recreation; it is not another lock,
//! generation counter, registry, or durable authority.

use std::sync::Arc;

use super::{SessionRuntimeSlot, SessionRuntimeSlots};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionRealizationPermitDecision {
    Admit,
    Retry,
}

/// The production post-wait decision. Tokio supplies mutual exclusion; this
/// kernel supplies the repository-owned rule that only the current slot's
/// acquired identity may enter a realization phase.
const fn session_realization_permit_decision(
    lock_acquired: bool,
    slot_identity_is_current: bool,
) -> SessionRealizationPermitDecision {
    if lock_acquired && slot_identity_is_current {
        SessionRealizationPermitDecision::Admit
    } else {
        SessionRealizationPermitDecision::Retry
    }
}

/// Proof-carrying lifetime for the one current Session realization mutex.
/// The guard is intentionally private: only `SessionRuntimeSlots` can mint it,
/// after comparing the acquired `Arc` with the current slot allocation.
pub(crate) struct SessionRealizationPermit {
    _identity: Arc<tokio::sync::Mutex<()>>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl SessionRuntimeSlots {
    /// Acquire the current slot's realization owner and atomically apply the
    /// caller's admission projection. If removal/recreation happened while
    /// awaiting Tokio, retry against the replacement instead of admitting the
    /// stale allocation.
    pub(crate) async fn acquire_realization_with<R>(
        &self,
        session: &str,
        mut on_admit: impl FnMut(&mut SessionRuntimeSlot) -> R,
    ) -> (SessionRealizationPermit, R) {
        loop {
            let identity = self.update(session, |slot| slot.realization.clone());
            let guard = identity.clone().lock_owned().await;
            let admitted = self
                .modify(session, |slot| {
                    let decision = session_realization_permit_decision(
                        true,
                        Arc::ptr_eq(&slot.realization, &identity),
                    );
                    (decision == SessionRealizationPermitDecision::Admit).then(|| on_admit(slot))
                })
                .flatten();
            if let Some(result) = admitted {
                return (
                    SessionRealizationPermit {
                        _identity: identity,
                        _guard: guard,
                    },
                    result,
                );
            }
        }
    }

    pub(crate) async fn acquire_realization(&self, session: &str) -> SessionRealizationPermit {
        self.acquire_realization_with(session, |_| ()).await.0
    }
}

#[cfg(kani)]
#[kani::proof]
fn session_realization_permit_admits_only_a_current_acquired_slot() {
    let lock_acquired: bool = kani::any();
    let slot_identity_is_current: bool = kani::any();
    let decision = session_realization_permit_decision(lock_acquired, slot_identity_is_current);

    assert_eq!(
        decision == SessionRealizationPermitDecision::Admit,
        lock_acquired && slot_identity_is_current
    );
    assert!(slot_identity_is_current || decision == SessionRealizationPermitDecision::Retry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permit_decision_rejects_unlocked_and_replaced_slot_identities() {
        // Cause/effect table: C1 the Tokio owner guard was acquired; C2 the
        // captured mutex Arc is still the current Session slot allocation.
        // Effect E1 admits the phase only for C1+C2; E2 retries every missing
        // guard or removed/recreated-slot row.
        //
        // | Rule | C1 | C2 | Effect |
        // |---|---|---|---|
        // | P1 | no | no/yes | E2 |
        // | P2 | yes | no | E2 |
        // | P3 | yes | yes | E1 |
        for (lock_acquired, slot_current, expected) in [
            (false, false, SessionRealizationPermitDecision::Retry),
            (false, true, SessionRealizationPermitDecision::Retry),
            (true, false, SessionRealizationPermitDecision::Retry),
            (true, true, SessionRealizationPermitDecision::Admit),
        ] {
            assert_eq!(
                session_realization_permit_decision(lock_acquired, slot_current),
                expected,
                "lock_acquired={lock_acquired}, slot_current={slot_current}"
            );
        }
    }
}
