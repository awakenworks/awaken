//! Process-local mutual exclusion for the co-located Session realization driver.
//!
//! The durable lease fences *other* processes, but one process legitimately
//! reaches the same phase driver from a request (create/update/Run admission)
//! and from the lifecycle supervisor's recovery sweep, both under the same
//! owner and incarnation. Without exclusion those two drivers interleave their
//! phases and repeat the same external MCP stage/publish/drain effect, which
//! the `SessionRealizationMutex` relation forbids. The Worker-side driver
//! already runs under the Runtime Host's Session realization permit; this is
//! the same rule for the co-located driver, not another authority: it grants no
//! lease, decides no phase, and survives no restart.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The post-wait admission rule. Tokio supplies the waiting; this kernel
/// supplies the registry-owned rule that only the identity still registered for
/// the Session may enter a realization phase, so a lock acquired through an
/// entry that was already released and replaced waits again.
const fn local_realization_entry_admits(registered_identity_is_current: bool) -> bool {
    registered_identity_is_current
}

#[derive(Default)]
pub(crate) struct LocalRealizationLocks(Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

/// Held for exactly one local realization pass. Dropping it releases the
/// Session's entry so the registry cannot grow with every Session ever driven.
pub(crate) struct LocalRealizationGuard<'a> {
    locks: &'a LocalRealizationLocks,
    session_id: String,
    identity: Arc<tokio::sync::Mutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl LocalRealizationLocks {
    /// Wait for this Session's local realization driver slot.
    pub(crate) async fn acquire(&self, session_id: &str) -> LocalRealizationGuard<'_> {
        loop {
            let identity = {
                let mut registry = self.0.lock().expect("local realization registry poisoned");
                registry.entry(session_id.to_string()).or_default().clone()
            };
            let guard = identity.clone().lock_owned().await;
            let current = {
                let registry = self.0.lock().expect("local realization registry poisoned");
                registry
                    .get(session_id)
                    .is_some_and(|registered| Arc::ptr_eq(registered, &identity))
            };
            if local_realization_entry_admits(current) {
                return LocalRealizationGuard {
                    locks: self,
                    session_id: session_id.to_string(),
                    identity,
                    guard: Some(guard),
                };
            }
        }
    }
}

impl Drop for LocalRealizationGuard<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        let mut registry = self
            .locks
            .0
            .lock()
            .expect("local realization registry poisoned");
        // Two strong references remain while another driver already took this
        // identity out of the registry's `entry`: only the last holder removes.
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

    #[test]
    fn only_the_registered_identity_admits_a_pass() {
        // Cause/effect decision table: C1 the acquired entry is still the one
        // registered for this Session. E1 admit the pass; E2 wait again.
        // Rules: R1=C1=>E1; R2=!C1=>E2.
        assert!(local_realization_entry_admits(true), "R1/E1");
        assert!(!local_realization_entry_admits(false), "R2/E2");
    }

    #[tokio::test]
    async fn one_session_admits_one_driver_while_another_session_proceeds() {
        // Cause/effect graph: C1 a request driver holds Session A; C2 the
        // recovery driver asks for A; C3 it asks for B instead; C4 the first
        // guard is dropped. E1 C2 waits; E2 C3 proceeds immediately; E3 C4
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
        drop(admitted);
        assert!(
            locks
                .0
                .lock()
                .expect("local realization registry poisoned")
                .is_empty(),
            "R4/E4"
        );
    }
}
