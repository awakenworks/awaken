//! Co-located identity source for the canonical Session realization permit.
//!
//! The durable lease fences *other* processes, but one process legitimately
//! reaches the same phase driver from a request (create/update/Run admission)
//! and from the lifecycle supervisor's recovery sweep, both under the same
//! owner and incarnation. Without exclusion those two drivers interleave their
//! phases and repeat the same external MCP stage/publish/drain effect, which
//! the `SessionRealizationMutex` relation forbids.
//!
//! The Worker-side driver feeds its Session runtime slot allocation to the
//! contract-owned permit; the co-located driver has no slot, so this registry
//! *is* its exclusion identity. Both mint through
//! `awaken_session_contract::SessionRealizationPermit::acquire`, so this is the
//! same rule and the same admission kernel, not another authority: it grants no
//! lease, decides no phase, and survives no restart.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_session_contract::SessionRealizationPermit;

#[derive(Default)]
pub(crate) struct LocalRealizationLocks(Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

/// Held for exactly one local realization pass, inside the minted permit.
/// Dropping the permit drops this, which releases the Session's entry so the
/// registry cannot grow with every Session ever driven.
struct LocalRealizationEntry {
    locks: Arc<LocalRealizationLocks>,
    session_id: String,
    identity: Arc<tokio::sync::Mutex<()>>,
    owner: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl LocalRealizationLocks {
    /// Wait for this Session's local realization driver slot and mint the
    /// canonical permit the phase driver requires.
    pub(crate) async fn acquire(self: &Arc<Self>, session_id: &str) -> SessionRealizationPermit {
        let locks = self;
        SessionRealizationPermit::acquire(
            session_id,
            move || {
                let identity = {
                    let mut registry = locks.0.lock().expect("local realization registry poisoned");
                    registry.entry(session_id.to_string()).or_default().clone()
                };
                let locks = Arc::clone(locks);
                let session_id = session_id.to_string();
                async move {
                    let owner = identity.clone().lock_owned().await;
                    // The permit retains the address, not another strong
                    // reference: the entry guard below keeps the allocation
                    // alive for its whole life, so the address cannot be
                    // recycled underneath the comparison, and the registry's
                    // release rule stays a two-holder count.
                    let address = Arc::as_ptr(&identity) as usize;
                    (
                        address,
                        LocalRealizationEntry {
                            locks,
                            session_id,
                            identity,
                            owner: Some(owner),
                        },
                    )
                }
            },
            |address| {
                // Only the identity still registered for the Session may enter
                // a phase: an entry that was already released and replaced is
                // stale evidence, so the contract kernel waits again.
                let registry = locks.0.lock().expect("local realization registry poisoned");
                registry
                    .get(session_id)
                    .is_some_and(|registered| Arc::as_ptr(registered) as usize == *address)
                    .then_some(())
            },
        )
        .await
        .0
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.0
            .lock()
            .expect("local realization registry poisoned")
            .is_empty()
    }
}

impl Drop for LocalRealizationEntry {
    fn drop(&mut self) {
        drop(self.owner.take());
        let mut registry = self
            .locks
            .0
            .lock()
            .expect("local realization registry poisoned");
        // Two strong references remain while another driver already took this
        // identity out of the registry's `entry`: the registry's own and this
        // guard's. Only the last holder removes.
        if registry
            .get(&self.session_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, &self.identity))
            && Arc::strong_count(&self.identity) == 2
        {
            registry.remove(&self.session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_session_admits_one_driver_while_another_session_proceeds() {
        // Cause/effect graph: C1 a request driver holds Session A; C2 the
        // recovery driver asks for A; C3 it asks for B instead; C4 the first
        // permit is dropped. E1 C2 waits; E2 C3 proceeds immediately; E3 C4
        // admits the waiter; E4 no Session keeps a registry entry afterwards.
        // Rules: R1=C1+C2=>E1; R2=C1+C3=>E2; R3=C1+C2+C4=>E3; R4=E3=>E4.
        let locks = Arc::new(LocalRealizationLocks::default());
        let held = locks.acquire("A").await;
        let other = locks.acquire("B").await;
        let waiting = locks.acquire("A");
        let mut waiting = std::pin::pin!(waiting);
        assert!(futures_util::poll!(waiting.as_mut()).is_pending(), "R1/E1");
        drop(other);
        drop(held);
        let admitted = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("R3/E3 second driver was not admitted");
        assert_eq!(admitted.session_id(), "A", "R3/E3");
        drop(admitted);
        assert!(locks.is_empty(), "R4/E4");
    }

    #[tokio::test]
    async fn two_concurrent_drivers_never_hold_the_same_session_at_once() {
        // Cause/effect graph: C1 sixteen tasks drive Session A concurrently
        // through the same registry; C2 each records the live driver count for
        // the whole span it holds the permit; C3 eight other tasks drive
        // Session B at the same time. E1 no Session ever observes two
        // simultaneous drivers; E2 every task completed a pass; E3 the registry
        // is empty once every permit is dropped.
        // Rules: R1=C1+C2=>E1+E2; R2=C1+C3=>E1 per Session; R3=E2=>E3.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let locks = Arc::new(LocalRealizationLocks::default());
        let live: Arc<HashMap<&str, AtomicUsize>> = Arc::new(
            [("A", AtomicUsize::new(0)), ("B", AtomicUsize::new(0))]
                .into_iter()
                .collect(),
        );
        let passes = Arc::new(AtomicUsize::new(0));
        let mut drivers = Vec::new();
        for index in 0..24 {
            let session = if index % 3 == 0 { "B" } else { "A" };
            let locks = Arc::clone(&locks);
            let live = Arc::clone(&live);
            let passes = Arc::clone(&passes);
            drivers.push(tokio::spawn(async move {
                let permit = locks.acquire(session).await;
                assert_eq!(permit.session_id(), session);
                let concurrent = live[session].fetch_add(1, Ordering::SeqCst) + 1;
                // Yield inside the exclusion: a second admitted driver would be
                // observed here, exactly where the repeated MCP effect happened.
                tokio::task::yield_now().await;
                let still = live[session].load(Ordering::SeqCst);
                live[session].fetch_sub(1, Ordering::SeqCst);
                passes.fetch_add(1, Ordering::SeqCst);
                drop(permit);
                (concurrent, still)
            }));
        }
        for driver in drivers {
            let (concurrent, still) = driver.await.expect("driver task panicked");
            assert_eq!((concurrent, still), (1, 1), "R1/E1 + R2/E1");
        }
        assert_eq!(passes.load(Ordering::SeqCst), 24, "R1/E2");
        assert!(locks.is_empty(), "R3/E3");
    }
}
