// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
//! The one Session realization exclusion, and the permit that proves it is held.
//!
//! The durable lease fences *other* processes. It cannot fence two drivers
//! inside one process: the request path (create/update/Run admission) and the
//! lifecycle supervisor's recovery sweep legitimately reach the same phase
//! driver under the same owner and incarnation. Two such drivers interleave
//! their phases and repeat the same external MCP stage/publish/drain effect,
//! which the `SessionRealizationMutex` relation forbids.
//!
//! [`drive_session_realization`](super::drive_session_realization) therefore
//! takes its Session from a [`SessionRealizationPermit`] instead of a bare id.
//! The permit's fields are private and [`SessionRealizationPermit::acquire`] is
//! its only constructor, so "start a phase driver for a Session without holding
//! that Session's exclusion" has no spelling that type-checks.
//!
//! What this module deliberately does *not* own is the lock. A contract layer
//! that took a Tokio runtime dependency would drag it into every bounded proof
//! built from this crate, and the two drivers do not even agree on what a
//! Session's exclusion identity *is*: the Worker side keys it on the Session
//! runtime slot allocation it already owns (the same `Arc` identity its
//! revocation selection compares), while the co-located driver has no slot and
//! keys a registry by Session id. Both hand their acquired identity and owner
//! to the one kernel and the one constructor below, so there is a single rule
//! and a single proof with two identity sources — not two rules.

use crate::SessionId;
use std::any::Any;
use std::future::Future;

/// The post-wait admission rule. The caller's async primitive supplies the
/// waiting; this kernel supplies the rule that only an acquired owner whose
/// identity is *still* the one registered for the Session may enter a phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionRealizationExclusionDecision {
    Admit,
    Retry,
}

/// Admit exactly the acquired-and-current row; every other row waits again.
/// A removed/recreated slot or a released/replaced registry entry is stale
/// evidence, never a second phase driver.
#[must_use]
pub const fn session_realization_exclusion_admits(
    exclusion_owner_acquired: bool,
    registered_identity_is_current: bool,
) -> SessionRealizationExclusionDecision {
    if exclusion_owner_acquired && registered_identity_is_current {
        SessionRealizationExclusionDecision::Admit
    } else {
        SessionRealizationExclusionDecision::Retry
    }
}

/// Proof-carrying lifetime for one Session's realization exclusion.
///
/// The fields are private and there is no `Default`, `Clone` or deserializer:
/// the only value of this type in existence is one minted by
/// [`SessionRealizationPermit::acquire`] after a real exclusion owner was
/// acquired and re-checked. It is `Send`/`Sync` because the phase driver holds
/// it across `.await` inside spawned tasks; unforgeability comes from the
/// closed constructor, not from thread affinity.
///
/// Dropping the permit drops the owner, which is what releases the exclusion —
/// the permit is the lifetime, not a copy of a decision made elsewhere.
pub struct SessionRealizationPermit {
    session_id: SessionId,
    /// Retained so the exclusion cannot be released while a phase runs, and so
    /// a registry-owning caller can release its entry from this value's `Drop`.
    _exclusion_owner: Box<dyn Any + Send + Sync>,
    _registered_identity: Box<dyn Any + Send + Sync>,
}

impl SessionRealizationPermit {
    /// Acquire `session_id`'s realization exclusion and mint its permit.
    ///
    /// `enter` yields the identity currently registered for the Session
    /// together with the acquired owner of that identity; it may wait. `admit`
    /// then re-reads the caller's registry under its own synchronous lock and
    /// returns `Some` only while `identity` is still the registered one,
    /// applying any projection the caller wants performed atomically with
    /// admission. A `None` answer means the registration was replaced while
    /// this call was waiting: the stale owner is dropped and the whole
    /// acquisition is retried against the replacement.
    pub async fn acquire<Identity, Owner, Enter, Entered, Admit, Admitted>(
        session_id: &SessionId,
        mut enter: Enter,
        mut admit: Admit,
    ) -> (Self, Admitted)
    where
        Identity: Send + Sync + 'static,
        Owner: Send + Sync + 'static,
        Enter: FnMut() -> Entered,
        Entered: Future<Output = (Identity, Owner)>,
        Admit: FnMut(&Identity) -> Option<Admitted>,
    {
        loop {
            let (identity, owner) = enter().await;
            let admitted = admit(&identity);
            match session_realization_exclusion_admits(true, admitted.is_some()) {
                SessionRealizationExclusionDecision::Admit => {
                    let admitted =
                        admitted.expect("the admitted decision carries the caller's projection");
                    return (
                        Self {
                            session_id: crate::SessionId::from(session_id.to_string()),
                            _exclusion_owner: Box::new(owner),
                            _registered_identity: Box::new(identity),
                        },
                        admitted,
                    );
                }
                SessionRealizationExclusionDecision::Retry => drop((owner, identity)),
            }
        }
    }

    /// The Session this permit excludes. The phase driver takes its Session
    /// from here, so a permit for another Session cannot drive this one.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Check a borrowed held exclusion against the adapter's current
    /// registration, without exposing its owner or acquiring another lock.
    #[must_use]
    pub fn registered_identity_matches<T: Send + Sync + 'static>(
        &self,
        check: impl FnOnce(&T) -> bool,
    ) -> bool {
        self._registered_identity
            .downcast_ref::<T>()
            .is_some_and(check)
    }
}

impl std::fmt::Debug for SessionRealizationPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionRealizationPermit")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn exclusion_decision_rejects_unlocked_and_replaced_identities() {
        // Cause/effect table: C1 the exclusion owner guard was acquired; C2 the
        // captured identity is still the registered one for this Session.
        // Effect E1 admits the phase only for C1+C2; E2 retries every missing
        // owner or removed/recreated-registration row.
        //
        // | Rule | C1 | C2 | Effect |
        // |---|---|---|---|
        // | P1 | no | no | E2 |
        // | P2 | no | yes | E2 |
        // | P3 | yes | no | E2 |
        // | P4 | yes | yes | E1 |
        for (acquired, current, expected) in [
            (false, false, SessionRealizationExclusionDecision::Retry),
            (false, true, SessionRealizationExclusionDecision::Retry),
            (true, false, SessionRealizationExclusionDecision::Retry),
            (true, true, SessionRealizationExclusionDecision::Admit),
        ] {
            assert_eq!(
                session_realization_exclusion_admits(acquired, current),
                expected,
                "acquired={acquired}, current={current}"
            );
        }
    }

    #[tokio::test]
    async fn a_replaced_registration_waits_again_instead_of_admitting_a_stale_owner() {
        // Cause/effect graph: C1 the first acquisition observes registration A;
        // C2 the registry is replaced with B before `admit` runs; C3 the second
        // acquisition observes B. E1 the stale A pass is not admitted; E2 the
        // permit is minted from B and names the Session; E3 `enter` ran twice.
        // Rules: R1=C1+C2=>E1; R2=C3=>E2; R3=E1=>E3.
        let registered = Arc::new(Mutex::new(1_u8));
        let entered = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&entered);
        let replace = Arc::clone(&registered);
        let (permit, admitted) = SessionRealizationPermit::acquire(
            &crate::SessionId::from("session-a"),
            || {
                let identity = *replace.lock().unwrap();
                observed.lock().unwrap().push(identity);
                // The first pass is overtaken between `enter` and `admit`.
                if identity == 1 {
                    *replace.lock().unwrap() = 2;
                }
                async move { (identity, ()) }
            },
            |identity| (*identity == *registered.lock().unwrap()).then_some(*identity),
        )
        .await;
        assert_eq!(admitted, 2, "R1/E1 + R2/E2");
        assert_eq!(permit.session_id(), "session-a", "R2/E2");
        assert_eq!(*entered.lock().unwrap(), vec![1, 2], "R3/E3");
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    /// @exhaustive bool(2) x bool(2) = 4
    /// @obligation session_realization.concurrent_phase_drivers_are_serialized_per_session
    /// @obligation session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex
    #[test]
    fn session_realization_permit_admits_only_a_current_acquired_slot() {
        for exclusion_owner_acquired in [false, true] {
            for registered_identity_is_current in [false, true] {
                let decision = session_realization_exclusion_admits(
                    exclusion_owner_acquired,
                    registered_identity_is_current,
                );
                assert_eq!(
                    decision == SessionRealizationExclusionDecision::Admit,
                    exclusion_owner_acquired && registered_identity_is_current
                );
                assert!(
                    registered_identity_is_current
                        || decision == SessionRealizationExclusionDecision::Retry
                );
            }
        }
    }
}
